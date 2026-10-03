//! Successful worker replies must pass through the TUI's production completion path.

use super::{test_config, Tui};
use crate::test_utils::{TestEnvironment, ENV_LOCK};
use futures_util::StreamExt;
use harnx_runtime::config::{NatsRouting, NatsServerConfig};
use harnx_runtime::nats_local_server::SharedNatsServer;
use harnx_runtime::nats_session::{NatsSession, NatsSessionConfig};
use harnx_runtime::operator_tools::OperatorToolCommand;
use harnx_runtime::SessionActivationRoute;
use serde_json::{json, Value};
use std::time::Duration;

struct CompletionWorker {
    tui: Tui,
    client: async_nats::Client,
    requests: async_nats::Subscriber,
    session_key: String,
    _broker: SharedNatsServer,
}

impl CompletionWorker {
    async fn start() -> Self {
        let broker = harnx_runtime::nats_local_server::ensure_shared_server()
            .await
            .unwrap();
        let config = test_config();
        {
            let mut cfg = config.write();
            cfg.nats_routing = NatsRouting::Cluster("completion".into());
            cfg.nats_servers = vec![serde_json::from_value::<NatsServerConfig>(json!({
                "name": "completion", "url": broker.url, "token": broker.token
            }))
            .unwrap()];
            cfg.remote_agent = Some(("completion-agent".into(), "completion".into()));
            let mut session =
                harnx_runtime::config::session::new(&cfg, "completion-session", None).unwrap();
            session.use_tools = Some(vec!["allowed_*".into()]);
            cfg.session = Some(session);
            cfg.nats_tool_declarations.write().push(
                serde_json::from_value(json!({
                    "name": "allowed_global_only", "description": "must not leak", "parameters": {}
                }))
                .unwrap(),
            );
        }
        let snapshot = config.read().clone();
        let session = NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: "completion".into(),
                initializer: harnx_runtime::SessionInitializer::named_from_config(
                    "completion-agent",
                    &snapshot,
                ),
                session_id: Some(snapshot.session.as_ref().unwrap().id().into()),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            &config,
            harnx_runtime::utils::create_abort_signal(),
        )
        .await
        .unwrap();
        let client = snapshot.nats_client("completion").await.unwrap();
        let requests = client
            .subscribe("cluster.completion.operator_tools")
            .await
            .unwrap();
        client.flush().await.unwrap();
        Self {
            tui: Tui::init(&config).await.unwrap(),
            client,
            requests,
            session_key: session.storage_key().into(),
            _broker: broker,
        }
    }

    async fn complete(
        &mut self,
        line: &str,
        output: &str,
        error: Option<&str>,
    ) -> Vec<(String, Option<String>)> {
        let response = async {
            let request = tokio::time::timeout(Duration::from_secs(5), self.requests.next())
                .await
                .expect("TUI must query the active worker")
                .unwrap();
            let payload: Value = serde_json::from_slice(&request.payload).unwrap();
            assert_eq!(payload["version"], 1);
            assert_eq!(payload["session_key"], self.session_key);
            assert_eq!(payload["use_tools"], json!(["allowed_*"]));
            assert_eq!(payload["json"], true);
            assert_eq!(
                serde_json::from_value::<OperatorToolCommand>(payload["command"].clone()).unwrap(),
                OperatorToolCommand::List { pattern: None }
            );
            // Mock only the worker boundary. Parsing, prefix filtering and projection
            // run through compute_completions, not a test-local implementation.
            self.client
                .publish(
                    request.reply.unwrap(),
                    serde_json::to_vec(&json!({
                        "type": "finished", "output": output, "error": error
                    }))
                    .unwrap()
                    .into(),
                )
                .await
                .unwrap();
            self.client.flush().await.unwrap();
        };
        let (candidates, ()) =
            tokio::join!(self.tui.compute_completions(line, line.len()), response);
        candidates
    }
}

#[tokio::test]
async fn operator_tool_completion_uses_active_worker_prefixes_and_descriptions() {
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut worker = CompletionWorker::start().await;
    // The worker returns its allowed inventory, not the frontend's global cache.
    let output = json!([
        {"name": "allowed_read", "description": "Read files, including écho"},
        {"name": "allowed_write", "description": "Write files"},
        {"name": "allowed_without_description"},
        {"name": "allowed_null_description", "description": null},
        {"name": "allowed_numeric_description", "description": 42},
        {"description": "missing name"},
        {"name": 42, "description": "invalid name"}
    ])
    .to_string();
    let all = vec![
        (
            "allowed_read".into(),
            Some("Read files, including écho".into()),
        ),
        ("allowed_write".into(), Some("Write files".into())),
        ("allowed_without_description".into(), None),
        ("allowed_null_description".into(), None),
        ("allowed_numeric_description".into(), None),
    ];
    for command in [".info tool", ".call tool", ".list tools"] {
        for (prefix, expected) in [
            ("", all.clone()),
            ("allowed_read", vec![all[0].clone()]),
            ("allowed_w", vec![all[1].clone(), all[2].clone()]),
            ("read", vec![]),
            ("allowed_absent", vec![]),
            ("allowed_global", vec![]),
        ] {
            let line = format!("{command} {prefix}");
            assert_eq!(
                worker.complete(&line, &output, None).await,
                expected,
                "{line}"
            );
        }
    }
}

#[tokio::test]
async fn operator_tool_completion_rejects_bad_replies_and_accepts_empty_inventory() {
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut worker = CompletionWorker::start().await;
    for command in [".info tool ", ".call tool "] {
        for (output, error) in [
            ("[]", None),
            ("not JSON", None),
            ("{}", None),
            (
                r#"[{"name":"allowed_read","description":"must not appear"}]"#,
                Some("worker failed"),
            ),
        ] {
            assert!(
                worker.complete(command, output, error).await.is_empty(),
                "{command} {output}"
            );
        }
    }
}
