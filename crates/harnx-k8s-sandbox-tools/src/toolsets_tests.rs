use super::*;
use crate::lifecycle::{CreateSandboxClaim, SandboxApi, SandboxCondition, SandboxRecord};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use harnx_runtime::nats_session_metadata::{SessionInitializer, SessionMetadata};
use harnx_toolset::{ToolInvocation, ToolSpec};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

struct TestNats {
    url: String,
    child: Child,
    _store: tempfile::TempDir,
}

impl Drop for TestNats {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_nats() -> Option<TestNats> {
    let binary = which::which("nats-server").ok()?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()?
        .port();
    let store = tempfile::tempdir().ok()?;
    let mut child = Command::new(binary)
        .args(["-js", "-sd"])
        .arg(store.path())
        .args(["-p", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let url = format!("nats://127.0.0.1:{port}");
    for _ in 0..50 {
        if async_nats::connect(&url).await.is_ok() {
            return Some(TestNats {
                url,
                child,
                _store: store,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[derive(Default)]
struct ReadyApi {
    seen_ids: Mutex<Vec<String>>,
    activity: Mutex<Vec<String>>,
    replica_updates: Mutex<Vec<(String, i64)>>,
    deletes: Mutex<Vec<String>>,
}

#[async_trait]
impl SandboxApi for ReadyApi {
    async fn create_claim(&self, request: CreateSandboxClaim) -> Result<String> {
        Ok(request.name)
    }

    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>> {
        self.seen_ids.lock().push(id.to_string());
        Ok(Some(SandboxRecord {
            id: id.to_string(),
            sandbox_name: Some(format!("pod-{id}")),
            pod_ips: vec!["10.0.0.8".to_string()],
            replicas: Some(1),
            conditions: vec![SandboxCondition {
                kind: "Ready".to_string(),
                status: "True".to_string(),
                reason: String::new(),
                message: String::new(),
            }],
            shutdown_time: None,
            created_at: Some(Utc::now()),
            last_activity: None,
        }))
    }

    async fn list(&self) -> Result<Vec<SandboxRecord>> {
        Ok(Vec::new())
    }

    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()> {
        self.replica_updates.lock().push((id.to_string(), replicas));
        Ok(())
    }

    async fn update_shutdown_time(
        &self,
        _id: &str,
        _shutdown_time: chrono::DateTime<Utc>,
    ) -> Result<()> {
        Ok(())
    }

    async fn bump_activity(&self, id: &str, _now: chrono::DateTime<Utc>) -> Result<()> {
        self.activity.lock().push(id.to_string());
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.deletes.lock().push(id.to_string());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingCaller {
    calls: Mutex<Vec<RecordedCall>>,
    disconnected: Mutex<Vec<String>>,
    responses: Mutex<VecDeque<Result<Value, McpCallError>>>,
    wait_for_cancellation: AtomicBool,
    call_started: Notify,
    cancellation_observed: AtomicBool,
}

struct RecordedCall {
    sandbox_id: String,
    endpoint: String,
    tool: String,
    args: Map<String, Value>,
    capabilities: BTreeSet<String>,
}

#[async_trait]
impl McpCaller for RecordingCaller {
    async fn call(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError> {
        self.calls.lock().push(RecordedCall {
            sandbox_id: sandbox_id.to_string(),
            endpoint: endpoint.to_string(),
            tool: tool.to_string(),
            args,
            capabilities,
        });
        self.call_started.notify_one();
        if self.wait_for_cancellation.load(Ordering::SeqCst) {
            cancel.cancelled().await;
            self.cancellation_observed.store(true, Ordering::SeqCst);
            return Err(McpCallError {
                kind: McpCallErrorKind::Cancelled,
                message: "cancelled by forwarded token".to_string(),
            });
        }
        self.responses
            .lock()
            .pop_front()
            .unwrap_or_else(|| Ok(json!({"content": [{"type": "text", "text": "proxied"}]})))
    }

    async fn disconnect(&self, sandbox_id: &str) {
        self.disconnected.lock().push(sandbox_id.to_string());
    }
}

struct ToolsetFixture {
    _nats: TestNats,
    api: Arc<ReadyApi>,
    caller: Arc<RecordingCaller>,
    metadata: SessionMetadataStore,
    bash: Arc<dyn Toolset>,
    sandbox: Arc<dyn Toolset>,
}

impl ToolsetFixture {
    async fn start(session_id: &str, binding: Option<&str>) -> Result<Option<Self>> {
        let Some(nats) = spawn_nats().await else {
            return Ok(None);
        };
        let client = async_nats::connect(&nats.url).await?;
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await?;
        metadata
            .create(&SessionMetadata::new(
                session_id,
                SessionInitializer::named("coder", Default::default()),
            ))
            .await?;
        if let Some(sandbox_id) = binding {
            metadata
                .replace_tool_context_value(
                    ToolContextEntry {
                        session_id,
                        key: SANDBOX_CONTEXT_KEY,
                    },
                    json!({"version": 1, "sandbox_id": sandbox_id}),
                )
                .await?;
        }
        let api = Arc::new(ReadyApi::default());
        let caller = Arc::new(RecordingCaller::default());
        let toolsets = sandbox_toolsets(
            SandboxManager::new(api.clone(), Default::default()),
            caller.clone(),
            metadata.clone(),
        );
        let find = |name| {
            toolsets
                .iter()
                .find(|toolset| toolset.name() == name)
                .cloned()
                .unwrap()
        };
        Ok(Some(Self {
            _nats: nats,
            api,
            caller,
            metadata,
            bash: find("bash"),
            sandbox: find("sandbox"),
        }))
    }
}

async fn bound_sandbox(
    metadata: &SessionMetadataStore,
    session_id: &str,
) -> Result<SandboxBinding> {
    Ok(serde_json::from_value(
        metadata.get_tool_context(session_id).await?.unwrap().values[SANDBOX_CONTEXT_KEY].clone(),
    )?)
}

#[test]
fn proxy_schema_adds_an_optional_sandbox_override() {
    let spec = proxy_spec(ToolSpec {
        cancellation_guarantee: Default::default(),
        name: "read".to_string(),
        description: String::new(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(30),
        meta: None,
    });

    assert_eq!(spec.input_schema["required"], json!(["path"]));
    assert_eq!(
        spec.input_schema["properties"]["sandbox_id"]["type"],
        "string"
    );
    assert_eq!(spec.timeout_secs, Some(0));
}

#[test]
fn proxy_tool_surface_matches_tartarus() {
    let bash = harnx_bash_tools::builtin_tool_specs()
        .into_iter()
        .map(proxy_spec)
        .map(|spec| format!("bash_{}", spec.name))
        .collect::<Vec<_>>();
    assert_eq!(
        bash,
        [
            "bash_exec",
            "bash_read_exec_log",
            "bash_spawn",
            "bash_wait",
            "bash_terminate",
            "bash_rollback_file",
        ]
    );

    let fs = harnx_fs_tools::builtin_tool_specs()
        .into_iter()
        .map(proxy_spec)
        .map(|spec| format!("fs_{}", spec.name))
        .collect::<Vec<_>>();
    assert_eq!(
        fs,
        [
            "fs_read",
            "fs_write",
            "fs_edit",
            "fs_insert",
            "fs_re_replace",
            "fs_ls",
            "fs_grep",
            "fs_find",
            "fs_rollback_file",
        ]
    );
}

#[test]
fn endpoint_formats_ipv4_and_ipv6_addresses() {
    assert_eq!(mcp_endpoint("10.0.0.8"), "http://10.0.0.8:8080/mcp");
    assert_eq!(mcp_endpoint("fd00::8"), "http://[fd00::8]:8080/mcp");
}

#[test]
fn lifecycle_status_is_explicitly_read_only() {
    let status = lifecycle_specs()
        .into_iter()
        .find(|spec| spec.name == "status")
        .unwrap();
    assert!(status.read_only_hint);
    assert!(status.idempotent_hint);
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_requires_or_resolves_an_ambient_session_binding() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };

    let missing = fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd"}),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(missing, ToolInvokeError::Recoverable(_)));
    assert!(missing.to_string().contains("no sandbox is bound"));

    let result = fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-1".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::from([
                    harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE.to_string(),
                ]),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    assert_eq!(result["content"][0]["text"], "proxied");
    assert_eq!(fixture.api.seen_ids.lock().as_slice(), ["claim-ambient"]);
    assert_eq!(fixture.api.activity.lock().as_slice(), ["claim-ambient"]);
    {
        let calls = fixture.caller.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].sandbox_id, "claim-ambient");
        assert_eq!(calls[0].endpoint, "http://10.0.0.8:8080/mcp");
        assert_eq!(calls[0].tool, "bash_exec");
        assert_eq!(
            calls[0].args,
            Map::from_iter([("command".to_string(), json!("pwd"))])
        );
        assert!(calls[0]
            .capabilities
            .contains(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_explicit_override_is_one_call_and_not_forwarded() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };
    fixture
        .bash
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "pwd", "sandbox_id": "claim-explicit"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-explicit".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    let calls = fixture.caller.calls.lock();
    assert_eq!(calls[0].sandbox_id, "claim-explicit");
    assert!(!calls[0].args.contains_key("sandbox_id"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_forwards_cancellation_after_the_mcp_call_starts() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-cancel", Some("claim-cancel")).await? else {
        return Ok(());
    };
    fixture
        .caller
        .wait_for_cancellation
        .store(true, Ordering::SeqCst);
    let call_started = fixture.caller.call_started.notified();
    let cancel = CancellationToken::new();
    let invocation_cancel = cancel.clone();
    let bash = fixture.bash.clone();
    let call = tokio::spawn(async move {
        bash.invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args: json!({"command": "sleep 30"}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-cancel".to_string(),
                invoking_session_id: Some("session-cancel".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: invocation_cancel,
        })
        .await
    });

    call_started.await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), call)
        .await??
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Fatal(_)));
    assert!(fixture.caller.cancellation_observed.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn release_uses_and_clears_the_ambient_binding() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-1", Some("claim-ambient")).await? else {
        return Ok(());
    };
    fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "release".to_string(),
            args: json!({}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-2".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.caller.disconnected.lock().as_slice(),
        ["claim-ambient"]
    );
    assert_eq!(
        fixture.api.replica_updates.lock().as_slice(),
        [("claim-ambient".to_string(), 0)]
    );

    fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "release".to_string(),
            args: json!({"destroy": true}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-3".to_string(),
                invoking_session_id: Some("session-1".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(fixture.api.deletes.lock().as_slice(), ["claim-ambient"]);
    assert!(!fixture
        .metadata
        .get_tool_context("session-1")
        .await?
        .unwrap()
        .values
        .contains_key(SANDBOX_CONTEXT_KEY));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_clones_after_a_retry_and_binds_the_session() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-clone", None).await? else {
        return Ok(());
    };
    fixture.caller.responses.lock().extend([
            Ok(json!({
                "isError": true,
                "content": [{"type": "text", "text": "execution_id: first\nexit_code: 128\nfatal: Repository not found."}]
            })),
            Ok(json!({
                "content": [{"type": "text", "text": "execution_id: second\nexit_code: 0\n<!-- start stdout -->\n```\nmain\n```\n<!-- end stdout -->"}]
            })),
        ]);
    let result = fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "connect".to_string(),
            args: json!({
                "sandbox_id": "claim-clone",
                "repos": [{"repo_url": "https://github.com/acme/widgets.git"}]
            }),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-clone".to_string(),
                invoking_session_id: Some("session-clone".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();

    assert_eq!(result["sandbox_id"], "claim-clone");
    assert_eq!(result["repos"][0]["clone_path"], "/workspace/widgets");
    assert_eq!(result["repos"][0]["branch"], "main");
    assert!(result["repos"][0].get("error").is_none());
    {
        let calls = fixture.caller.calls.lock();
        assert_eq!(calls.len(), 2);
        assert!(calls
            .iter()
            .all(|call| call.tool == "bash_exec" && call.sandbox_id == "claim-clone"));
    }
    let binding = bound_sandbox(&fixture.metadata, "session-clone").await?;
    assert_eq!(binding.sandbox_id, "claim-clone");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_binds_before_a_cancelled_clone_returns() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start("session-cancelled-clone", None).await? else {
        return Ok(());
    };
    fixture.caller.responses.lock().push_back(Err(McpCallError {
        kind: McpCallErrorKind::Cancelled,
        message: "cancelled clone".to_string(),
    }));

    let error = fixture
        .sandbox
        .invoke_with_context(ToolInvocation {
            tool: "connect".to_string(),
            args: json!({
                "sandbox_id": "claim-cancelled-clone",
                "repos": [{"repo_url": "https://github.com/acme/widgets.git"}]
            }),
            context: ToolInvocationContext {
                operation: None,
                call_id: "call-cancelled-clone".to_string(),
                invoking_session_id: Some("session-cancelled-clone".to_string()),
                capabilities: BTreeSet::new(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, ToolInvokeError::Fatal(_)));
    let binding = bound_sandbox(&fixture.metadata, "session-cancelled-clone").await?;
    assert_eq!(binding.sandbox_id, "claim-cancelled-clone");
    Ok(())
}
