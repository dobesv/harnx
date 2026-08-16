use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_hookset::{FailPolicy, Hook, HookSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const SERVER_NAME: &str = "proxy-auth";

pub use harnx_hookset::HARNX_HOOK_NAME as HARNX_HOOK_NAME_ENV;

#[derive(Deserialize)]
struct HookRequest {
    id: String,
    #[serde(default)]
    tool_input: Option<Value>,
}

#[derive(Serialize)]
struct HookResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Serialize)]
struct HookSpecificOutput {
    #[serde(rename = "toolInput")]
    tool_input: Value,
}

/// Persistent proxy and startup environment exposed as a NATS PreToolUse hook.
pub struct ProxyAuthHook {
    name: String,
    proxy_port: u16,
    ca_cert_path: String,
    extra_env: Map<String, Value>,
}

impl ProxyAuthHook {
    pub fn new(
        name: String,
        proxy_port: u16,
        ca_cert_path: PathBuf,
        extra_env: Map<String, Value>,
    ) -> Self {
        Self {
            name,
            proxy_port,
            ca_cert_path: ca_cert_path.to_string_lossy().into_owned(),
            extra_env,
        }
    }
}

#[async_trait]
impl Hook for ProxyAuthHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn hooks(&self) -> Vec<HookSpec> {
        vec![HookSpec {
            event: "PreToolUse".to_string(),
            matcher: Some("exec|spawn".to_string()),
            priority: 0,
            timeout_secs: None,
            fail_policy: FailPolicy::Closed,
        }]
    }

    async fn handle_hook(&self, payload: HookPayload) -> HookOutcome {
        let mutated_tool_input = match payload.hook_event {
            HookEvent::PreToolUse { tool_input, .. } => Some(augment_tool_input(
                Some(tool_input),
                self.proxy_port,
                &self.ca_cert_path,
                &self.extra_env,
            )),
            _ => None,
        };

        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                mutated_tool_input,
                ..HookResult::default()
            },
        }
    }
}

pub async fn run_jsonl_loop(
    proxy_port: u16,
    ca_cert_path: PathBuf,
    extra_env: Map<String, Value>,
    mut notice_rx: crate::notice::HookNoticeReceiver,
) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let ca_cert_path = ca_cert_path.to_string_lossy().into_owned();

    // This is the sole writer of proxy-auth's stdout, so hook responses and
    // async notice lines never interleave mid-line.
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if line.is_empty() {
                    continue;
                }

                let request: HookRequest = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(e) => {
                        log::warn!("Malformed JSONL input (skipping): {e}");
                        continue;
                    }
                };
                // Respond to any PreToolUse event with a tool_input — tool name
                // filtering is the caller's responsibility (the hook matcher).
                let response = if request.tool_input.is_some() {
                    HookResponse {
                        id: request.id,
                        hook_specific_output: Some(HookSpecificOutput {
                            tool_input: augment_tool_input(
                                request.tool_input,
                                proxy_port,
                                &ca_cert_path,
                                &extra_env,
                            ),
                        }),
                    }
                } else {
                    HookResponse {
                        id: request.id,
                        hook_specific_output: None,
                    }
                };

                let encoded = serde_json::to_string(&response)?;
                stdout.write_all(encoded.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
            Some(hook_notice) = notice_rx.recv() => {
                let encoded = crate::notice::to_line(&hook_notice);
                stdout.write_all(encoded.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
    }

    Ok(())
}

fn augment_tool_input(
    tool_input: Option<Value>,
    proxy_port: u16,
    ca_cert_path: &str,
    extra_env: &Map<String, Value>,
) -> Value {
    let mut tool_input = match tool_input {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    let mut env = match tool_input.remove("env") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    for (key, value) in extra_env {
        env.entry(key.clone()).or_insert_with(|| value.clone());
    }

    for (key, value) in [
        ("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}")),
        ("HTTPS_PROXY", format!("http://127.0.0.1:{proxy_port}")),
        ("SSL_CERT_FILE", ca_cert_path.to_owned()),
        ("NODE_EXTRA_CA_CERTS", ca_cert_path.to_owned()),
        ("CURL_CA_BUNDLE", ca_cert_path.to_owned()),
        ("REQUESTS_CA_BUNDLE", ca_cert_path.to_owned()),
        ("GIT_SSL_CAINFO", ca_cert_path.to_owned()),
    ] {
        env.entry(key.to_owned()).or_insert(Value::String(value));
    }

    tool_input.insert("env".to_owned(), Value::Object(env));
    Value::Object(tool_input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROXY_PORT: u16 = 4312;
    const CA_CERT_PATH: &str = "/tmp/test-ca.pem";

    fn expected_proxy_env() -> Value {
        json!({
            "HTTP_PROXY": format!("http://127.0.0.1:{PROXY_PORT}"),
            "HTTPS_PROXY": format!("http://127.0.0.1:{PROXY_PORT}"),
            "SSL_CERT_FILE": CA_CERT_PATH,
            "NODE_EXTRA_CA_CERTS": CA_CERT_PATH,
            "CURL_CA_BUNDLE": CA_CERT_PATH,
            "REQUESTS_CA_BUNDLE": CA_CERT_PATH,
            "GIT_SSL_CAINFO": CA_CERT_PATH,
        })
    }

    #[test]
    fn bash_exec_injects_proxy_env_vars() {
        let input = Some(json!({"command": "echo hi", "env": {"FOO": "bar"}}));

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &Map::new());

        assert_eq!(
            actual,
            json!({
                "command": "echo hi",
                "env": {
                    "FOO": "bar",
                    "HTTP_PROXY": format!("http://127.0.0.1:{PROXY_PORT}"),
                    "HTTPS_PROXY": format!("http://127.0.0.1:{PROXY_PORT}"),
                    "SSL_CERT_FILE": CA_CERT_PATH,
                    "NODE_EXTRA_CA_CERTS": CA_CERT_PATH,
                    "CURL_CA_BUNDLE": CA_CERT_PATH,
                    "REQUESTS_CA_BUNDLE": CA_CERT_PATH,
                    "GIT_SSL_CAINFO": CA_CERT_PATH,
                }
            })
        );
    }

    #[test]
    fn bash_spawn_injects_proxy_env_vars() {
        let input = Some(json!({"command": "sleep 1", "working_dir": "/tmp"}));

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &Map::new());

        assert_eq!(
            actual,
            json!({
                "command": "sleep 1",
                "working_dir": "/tmp",
                "env": expected_proxy_env(),
            })
        );
    }

    #[test]
    fn non_bash_tool_response_has_no_hook_specific_output() {
        let response = HookResponse {
            id: "test".to_owned(),
            hook_specific_output: None,
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"id": "test"})
        );
    }

    #[test]
    fn existing_env_key_is_preserved() {
        let input = Some(json!({
            "command": "echo hi",
            "env": {
                "HTTPS_PROXY": "http://user-proxy:9999",
                "SSL_CERT_FILE": "/custom/ca.pem"
            }
        }));

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &Map::new());

        assert_eq!(
            actual["env"]["HTTPS_PROXY"],
            json!("http://user-proxy:9999")
        );
        assert_eq!(actual["env"]["SSL_CERT_FILE"], json!("/custom/ca.pem"));
        assert_eq!(actual["env"]["NODE_EXTRA_CA_CERTS"], json!(CA_CERT_PATH));
    }

    #[test]
    fn missing_env_field_creates_env_with_proxy_vars() {
        let input = Some(json!({"command": "git fetch"}));

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &Map::new());

        assert_eq!(actual["env"], expected_proxy_env());
    }

    #[test]
    fn extra_env_is_injected_when_missing_from_tool_input_env() {
        let input = Some(json!({"command": "echo hi"}));
        let extra_env = Map::from_iter([(String::from("FOO"), json!("from-script"))]);

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &extra_env);

        assert_eq!(actual["env"]["FOO"], json!("from-script"));
        assert_eq!(
            actual["env"]["HTTPS_PROXY"],
            json!(format!("http://127.0.0.1:{PROXY_PORT}"))
        );
    }

    #[test]
    fn tool_input_env_takes_precedence_over_extra_env() {
        let input = Some(json!({
            "command": "echo hi",
            "env": {
                "FOO": "from-user"
            }
        }));
        let extra_env = Map::from_iter([(String::from("FOO"), json!("from-script"))]);

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &extra_env);

        assert_eq!(actual["env"]["FOO"], json!("from-user"));
    }

    #[test]
    fn extra_env_takes_precedence_over_proxy_defaults() {
        let input = Some(json!({"command": "echo hi"}));
        let extra_env = Map::from_iter([(
            String::from("HTTPS_PROXY"),
            json!("http://script-proxy:7777"),
        )]);

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &extra_env);

        assert_eq!(
            actual["env"]["HTTPS_PROXY"],
            json!("http://script-proxy:7777")
        );
        assert_eq!(
            actual["env"]["HTTP_PROXY"],
            json!(format!("http://127.0.0.1:{PROXY_PORT}"))
        );
    }

    #[test]
    fn gcp_metadata_env_from_extra_env_is_injected() {
        let input = Some(json!({"command": "node app.js"}));
        let extra_env = Map::from_iter([
            (
                String::from("GCE_METADATA_HOST"),
                json!(format!("127.0.0.1:{PROXY_PORT}")),
            ),
            (
                String::from("METADATA_SERVER_DETECTION"),
                json!("assume-present"),
            ),
            (String::from("GOOGLE_CLOUD_PROJECT"), json!("demo-project")),
        ]);

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &extra_env);

        assert_eq!(
            actual["env"]["GCE_METADATA_HOST"],
            json!(format!("127.0.0.1:{PROXY_PORT}"))
        );
        assert_eq!(
            actual["env"]["METADATA_SERVER_DETECTION"],
            json!("assume-present")
        );
        assert_eq!(actual["env"]["GOOGLE_CLOUD_PROJECT"], json!("demo-project"));
        assert_eq!(
            actual["env"]["HTTPS_PROXY"],
            json!(format!("http://127.0.0.1:{PROXY_PORT}"))
        );
        assert!(actual["env"]
            .get("GOOGLE_APPLICATION_CREDENTIALS")
            .is_none());
    }

    #[test]
    fn gcp_metadata_env_is_absent_without_extra_env() {
        let input = Some(json!({"command": "node app.js"}));

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &Map::new());

        assert!(actual["env"].get("GCE_METADATA_HOST").is_none());
        assert!(actual["env"].get("METADATA_SERVER_DETECTION").is_none());
        assert!(actual["env"].get("GOOGLE_CLOUD_PROJECT").is_none());
        assert!(actual["env"]
            .get("GOOGLE_APPLICATION_CREDENTIALS")
            .is_none());
    }

    #[test]
    fn tool_input_env_takes_precedence_over_gcp_metadata_env() {
        let input = Some(json!({
            "command": "node app.js",
            "env": {
                "GCE_METADATA_HOST": "operator-host:9000",
                "METADATA_SERVER_DETECTION": "ping-only",
                "GOOGLE_CLOUD_PROJECT": "operator-project"
            }
        }));
        let extra_env = Map::from_iter([
            (
                String::from("GCE_METADATA_HOST"),
                json!(format!("127.0.0.1:{PROXY_PORT}")),
            ),
            (
                String::from("METADATA_SERVER_DETECTION"),
                json!("assume-present"),
            ),
            (String::from("GOOGLE_CLOUD_PROJECT"), json!("flag-project")),
        ]);

        let actual = augment_tool_input(input, PROXY_PORT, CA_CERT_PATH, &extra_env);

        assert_eq!(
            actual["env"]["GCE_METADATA_HOST"],
            json!("operator-host:9000")
        );
        assert_eq!(
            actual["env"]["METADATA_SERVER_DETECTION"],
            json!("ping-only")
        );
        assert_eq!(
            actual["env"]["GOOGLE_CLOUD_PROJECT"],
            json!("operator-project")
        );
    }

    #[test]
    fn jsonl_round_trip_augments_bash_request() {
        let line = r#"{"id":"req-1","tool_name":"bash_exec","tool_input":{"command":"curl https://example.com","env":{"FOO":"bar"}}}"#;

        let request: HookRequest = serde_json::from_str(line).unwrap();
        let response = HookResponse {
            id: request.id,
            hook_specific_output: Some(HookSpecificOutput {
                tool_input: augment_tool_input(
                    request.tool_input,
                    PROXY_PORT,
                    CA_CERT_PATH,
                    &Map::new(),
                ),
            }),
        };

        let encoded = serde_json::to_string(&response).unwrap();
        let output: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(output["id"], json!("req-1"));
        assert_eq!(
            output["hookSpecificOutput"]["toolInput"]["command"],
            json!("curl https://example.com")
        );
        assert_eq!(
            output["hookSpecificOutput"]["toolInput"]["env"]["FOO"],
            json!("bar")
        );
        assert_eq!(
            output["hookSpecificOutput"]["toolInput"]["env"]["HTTPS_PROXY"],
            json!(format!("http://127.0.0.1:{PROXY_PORT}"))
        );
    }
}
