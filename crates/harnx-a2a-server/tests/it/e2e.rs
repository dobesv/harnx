//! Binary-boundary coverage: CLI/bootstrap, broker routing and real HTTP clients.
use crate::{
    streaming::{finish, snapshot_text, until_artifact, Frames},
    support::{binary, Harness, Script, DEADLINE},
};
use anyhow::{bail, Context, Result};
use futures::TryStreamExt;
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde_json::{json, Value};
use std::{
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio_util::task::AbortOnDropHandle;

const REAP_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Process {
    child: Box<dyn ChildWrapper>,
    name: &'static str,
}
impl Process {
    fn spawn(name: &'static str, command: Command) -> Result<Self> {
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::std::JobObject);
        Ok(Self {
            child: command.spawn().with_context(|| format!("spawn {name}"))?,
            name,
        })
    }
    async fn wait_success(&mut self) -> Result<()> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    anyhow::ensure!(status.success(), "{} exited: {status}", self.name);
                    return Ok(());
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .with_context(|| format!("{} (pid {}) exit deadline", self.name, self.child.id()))?
    }
    fn cleanup(&mut self) -> Result<()> {
        // Kill the whole group/job even if its leader has exited, so descendants
        // can't outlive the test. start_kill(), unlike kill(), doesn't wait.
        if let Err(error) = self.child.start_kill() {
            if self.child.try_wait()?.is_none() {
                return Err(error.into());
            }
        }
        // process-wrap 10's Windows try_wait() consumes completion-port events
        // without caching them. A subsequent wrapped wait() can block forever
        // on the drained port. Polling also reaps the native child on Unix.
        poll_exit(REAP_DEADLINE, || self.child.try_wait())?;
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            let message = format!(
                "{} (pid {}) cleanup failed: {error:#}",
                self.name,
                self.child.id()
            );
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

fn poll_exit(
    timeout: Duration,
    mut poll: impl FnMut() -> std::io::Result<Option<ExitStatus>>,
) -> Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = poll()? {
            return Ok(status);
        }
        anyhow::ensure!(
            started.elapsed() < timeout,
            "child reaping deadline ({timeout:?})"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[tokio::test]
async fn cli_exit_can_be_polled_repeatedly_before_cleanup() -> Result<()> {
    harnx_core::require_nextest();
    let mut command = Command::new(binary("harnx")?);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut process = Process::spawn("harnx --version", command)?;
    process.wait_success().await?;
    // Drain all queued Windows job notifications before Drop, reproducing the
    // old try_wait() -> wait() hang even for a child with no descendants.
    for _ in 0..16 {
        assert!(process
            .child
            .try_wait()?
            .context("CLI exit status lost")?
            .success());
    }
    drop(process);
    Ok(())
}

#[test]
fn process_reaping_has_a_deadline() {
    let error = poll_exit(Duration::ZERO, || Ok(None)).unwrap_err();
    assert_eq!(error.to_string(), "child reaping deadline (0ns)");
}

struct Server {
    // Reap the frontend before stopping its worker/broker or removing config.
    _process: Process,
    h: Harness,
    client: reqwest::Client,
    base: String,
}
impl Server {
    async fn start() -> Result<Self> {
        let h = Harness::start(Script::Text).await?;
        let agent = harnx_runtime::config::Config::agent_file("pkg/agent");
        std::fs::create_dir_all(agent.parent().context("package agent directory")?)?;
        std::fs::write(agent, "---\nmodel: /mock:test\ndescription: Package e2e agent\nversion: '1'\n---\nTest package agent\n")?;
        let log_path = h.config_dir().join("a2a.log");
        let log = std::fs::File::create(&log_path)?;
        let mut command = Command::new(binary("harnx-a2a-server")?);
        command
            .arg("--config-dir")
            .arg(h.config_dir())
            .args([
                "--cluster",
                "runner",
                "--agent",
                "runner",
                "--agent",
                "alias=pkg/agent",
                "--user-id-header",
                "X-User-Id",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
            ])
            .env("RUST_LOG", "harnx_a2a_server=info")
            .env_remove("HARNX_A2A_AGENTS")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let mut process = Process::spawn("A2A server", command)?;
        // Port 0 avoids the bind-close-rebind race. Read the bound address from
        // the production startup log instead of reserving a port ourselves.
        let base = tokio::time::timeout(DEADLINE, async {
            loop {
                let logs = std::fs::read_to_string(&log_path)?;
                if let Some(line) = logs.lines().find(|line| line.contains("serving A2A HTTP")) {
                    if let Some((_, address)) = line.split_once("127.0.0.1:") {
                        let port: String =
                            address.chars().take_while(char::is_ascii_digit).collect();
                        anyhow::ensure!(!port.is_empty(), "missing bound port: {line}");
                        return Ok(format!("http://127.0.0.1:{port}"));
                    }
                }
                if let Some(status) = process.child.try_wait()? {
                    bail!("A2A startup exited {status}: {logs}");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("A2A startup deadline")??;
        Ok(Self {
            _process: process,
            h,
            client: reqwest::Client::builder().timeout(DEADLINE).build()?,
            base,
        })
    }
    fn request(&self, call: RpcCall<'_>) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/agents/{}", self.base, call.export))
            .header("X-User-Id", call.owner)
            .header("A2A-Version", "1.0")
            .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":call.method,"params":call.params}))
    }
    async fn rpc(&self, call: RpcCall<'_>) -> Result<Value> {
        let response = self.request(call).send().await?;
        assert_eq!(response.status(), 200);
        let value: Value = response.json().await?;
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "stream-request");
        Ok(value)
    }
    async fn stream(&self, export: &str, method: &str, params: Value) -> Result<Frames> {
        Frames::from_response(
            self.request(RpcCall::new(export, "alice", method, params))
                .send()
                .await?,
        )
    }
    async fn requested(&self, count: usize) -> Result<()> {
        tokio::time::timeout(DEADLINE, async {
            while self.h.llm.requests.lock().len() < count {
                self.h.llm.requested.notified().await;
            }
        })
        .await
        .context("LLM request deadline")?;
        Ok(())
    }
    async fn blocking(&self, export: &str, message: Value, count: usize) -> Result<Value> {
        let request = self.request(RpcCall::new(
            export,
            "alice",
            "SendMessage",
            json!({"message": message}),
        ));
        let mut pending = AbortOnDropHandle::new(tokio::spawn(request.send()));
        self.requested(count).await?;
        assert!(
            !pending.is_finished(),
            "blocking send returned before LLM finished"
        );
        self.h.llm.release.notify_one();
        let response = tokio::time::timeout(DEADLINE, &mut pending).await???;
        assert_eq!(response.status(), 200);
        let envelope: Value = response.json().await?;
        assert!(envelope["error"].is_null(), "{envelope}");
        let task = envelope["result"]["task"].clone();
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(snapshot_text(&task), "Hello world");
        Ok(task)
    }
    async fn completed(&self, export: &str, id: &Value) -> Result<Value> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let envelope = self
                    .rpc(RpcCall::new(export, "alice", "GetTask", json!({"id":id})))
                    .await?;
                assert!(envelope["error"].is_null(), "{envelope}");
                let task = &envelope["result"];
                match task["status"]["state"].as_str() {
                    Some("TASK_STATE_COMPLETED") => return Ok(task.clone()),
                    Some("TASK_STATE_WORKING" | "TASK_STATE_SUBMITTED") => {}
                    _ => bail!("unexpected task state: {task}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("task completion deadline")?
    }
    async fn keys(&self, context: &Value) -> Result<Vec<String>> {
        let storage_key =
            harnx_core::session_identity::session_key(Some("pkg/agent"), context.as_str().unwrap());
        let prefix = format!("sessions/{storage_key}/");
        tokio::time::timeout(DEADLINE, async {
            Ok(self
                .h
                .metadata
                .kv_store()
                .keys()
                .await?
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .filter(|key| key.starts_with(&prefix))
                .collect())
        })
        .await
        .with_context(|| format!("session KV key listing deadline: {prefix}"))?
    }
    async fn delete(&self, context: &Value) -> Result<()> {
        let mut command = Command::new(binary("harnx")?);
        command
            .args([
                "delete",
                "session",
                context.as_str().unwrap(),
                "--agent",
                "pkg/agent",
                "--cluster",
                "runner",
            ])
            .stdin(std::process::Stdio::null());
        Process::spawn("harnx delete session", command)?
            .wait_success()
            .await
    }
}

struct RpcCall<'a> {
    export: &'a str,
    owner: &'a str,
    method: &'a str,
    params: Value,
}

impl<'a> RpcCall<'a> {
    fn new(export: &'a str, owner: &'a str, method: &'a str, params: Value) -> Self {
        Self {
            export,
            owner,
            method,
            params,
        }
    }
}

fn chat(id: &str, context: Option<&Value>) -> Value {
    let mut message = json!({"messageId":id,"role":"ROLE_USER","parts":[{"text":id}]});
    if let Some(context) = context {
        message["contextId"] = context.clone();
    }
    message
}

async fn fetch_discovery_card(server: &Server, export: &str, path: &str) -> Result<Value> {
    // No identity or protocol headers: discovery must stay public.
    let response = server
        .client
        .get(format!(
            "{}/agents/{export}/.well-known/{path}",
            server.base
        ))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    Ok(response.json().await?)
}

fn assert_discovery_card(card: &Value, base: &str, expected_public_name: &str) {
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("{base}/agents/{expected_public_name}")
    );
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert_eq!(card["capabilities"]["streaming"], true);
}

fn assert_same_package_card(expected: &mut Option<Value>, card: Value) {
    if let Some(expected) = expected {
        assert_eq!(&card, expected);
    }
    *expected = Some(card);
}

async fn discovery(server: &Server) -> Result<()> {
    let mut package_card = None;
    let targets = [
        ("alias", "alias", true),
        ("pkg__agent", "alias", true),
        ("pkg%2Fagent", "alias", true),
        ("runner", "runner", false),
    ];
    for (export, public_name, is_package) in targets {
        for path in ["agent-card.json", "agent.json"] {
            let card = fetch_discovery_card(server, export, path).await?;
            assert_discovery_card(&card, &server.base, public_name);
            if is_package {
                assert_same_package_card(&mut package_card, card);
            }
        }
    }
    Ok(())
}

async fn rejected_resumes(server: &Server, task: &Value) -> Result<()> {
    let context = &task["contextId"];
    for call in [
        RpcCall::new(
            "alias",
            "alice",
            "SendMessage",
            json!({"message":chat("unknown", Some(&json!("unknown-context")))}),
        ),
        RpcCall::new(
            "alias",
            "bob",
            "SendMessage",
            json!({"message":chat("foreign", Some(context))}),
        ),
        RpcCall::new(
            "runner",
            "alice",
            "SendMessage",
            json!({"message":chat("cross-export", Some(context))}),
        ),
        RpcCall::new("alias", "bob", "GetTask", json!({"id":task["id"]})),
        RpcCall::new("alias", "bob", "CancelTask", json!({"id":task["id"]})),
        RpcCall::new("alias", "bob", "SubscribeToTask", json!({"id":task["id"]})),
        RpcCall::new("alias", "bob", "ListTasks", json!({"contextId":context})),
    ] {
        let method = call.method;
        let error = server.rpc(call).await?;
        assert_eq!(error["error"]["code"], -32001, "{method}: {error}");
    }
    Ok(())
}

async fn assert_unauthorized_list_tasks(server: &Server) -> Result<()> {
    let unauthorized = server
        .client
        .post(format!("{}/agents/alias", server.base))
        .json(&json!({"jsonrpc":"2.0","id":"no-identity","method":"ListTasks","params":{}}))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(unauthorized.json::<Value>().await?["error"]["code"], -32000);
    Ok(())
}

async fn execute_jira_assignment_turn(
    server: &Server,
    assignment: &Value,
) -> Result<(Value, String)> {
    let first = server
        .blocking("pkg%2Fagent", assignment.clone(), 1)
        .await?;
    // Decode contents before matching so JSON escaping doesn't weaken the check.
    let first_messages = server.h.llm.requests.lock()[0]["messages"].clone();
    let first_contents = first_messages
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let banner = "--- A2A data part (mediaType: application/json) ---\n```json\n";
    let end = "\n```\n--- End A2A data part ---";
    let (_, data) = first_contents
        .split_once(banner)
        .context("missing data banner")?;
    let (data, _) = data.split_once(end).context("missing data end banner")?;
    // ProtoJSON may reorder object keys at the HTTP boundary. Check all fields,
    // then require the exact recorded block to survive in turn 2's transcript.
    assert_eq!(
        serde_json::from_str::<Value>(data)?,
        assignment["parts"][1]["data"]
    );
    let rendered_data = format!("{banner}{data}{end}");
    assert!(first_contents.contains(assignment["parts"][0]["text"].as_str().unwrap()));
    assert_eq!(first["history"][0]["parts"], assignment["parts"]);
    Ok((first, rendered_data))
}

async fn assert_assignment_dedupe_and_rejected_resumes(
    server: &Server,
    first: &Value,
    assignment: &Value,
) -> Result<()> {
    let retry = server
        .rpc(RpcCall::new(
            "alias",
            "alice",
            "SendMessage",
            json!({"message":assignment}),
        ))
        .await?;
    assert_eq!(retry["result"]["task"]["id"], first["id"]);
    assert_eq!(
        retry["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    rejected_resumes(server, first).await?;
    assert_eq!(server.h.llm.requests.lock().len(), 1);
    Ok(())
}

async fn submit_context_follow_up(server: &Server, first: &Value) -> Result<(Value, Value)> {
    let context = &first["contextId"];
    let follow_up = chat("chat-follow-up-with-context-only", Some(context));
    assert!(follow_up.get("taskId").is_none());
    assert!(follow_up["parts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p.get("data").is_none()));
    let params = json!({"message":follow_up,"configuration":{"returnImmediately":true}});
    let second = server
        .rpc(RpcCall::new(
            "pkg__agent",
            "alice",
            "SendMessage",
            params.clone(),
        ))
        .await?["result"]["task"]
        .clone();
    assert_eq!(second["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(&second["contextId"], context);
    assert_ne!(second["id"], first["id"]);
    Ok((second, params))
}

async fn assert_follow_up_transcript(
    server: &Server,
    rendered_data: &str,
    assignment_text: &str,
) -> Result<()> {
    server.requested(2).await?;
    let second_messages = server.h.llm.requests.lock()[1]["messages"].clone();
    let second_contents = second_messages
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        second_contents.contains(rendered_data),
        "turn 1 data missing from turn 2 transcript: {second_messages}"
    );
    assert!(second_contents.contains(assignment_text));
    assert!(second_contents.contains("chat-follow-up-with-context-only"));
    Ok(())
}

async fn assert_follow_up_retries(server: &Server, second: &Value, params: Value) -> Result<()> {
    let live_retry = server
        .rpc(RpcCall::new(
            "alias",
            "alice",
            "SendMessage",
            params.clone(),
        ))
        .await?;
    assert_eq!(live_retry["result"]["task"]["id"], second["id"]);
    assert_eq!(
        live_retry["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    server.h.llm.release.notify_one();
    server.completed("alias", &second["id"]).await?;
    let terminal_retry = server
        .rpc(RpcCall::new("pkg%2Fagent", "alice", "SendMessage", params))
        .await?;
    assert_eq!(terminal_retry["result"]["task"]["id"], second["id"]);
    assert_eq!(
        terminal_retry["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(server.h.llm.requests.lock().len(), 2);
    Ok(())
}

async fn execute_context_follow_up_turn(
    server: &Server,
    first: &Value,
    rendered_data: &str,
    assignment_text: &str,
) -> Result<Value> {
    let (second, params) = submit_context_follow_up(server, first).await?;
    assert_follow_up_transcript(server, rendered_data, assignment_text).await?;
    assert_follow_up_retries(server, &second, params).await?;
    Ok(second)
}

async fn assert_scoped_list_and_unrelated_tasks(
    server: &Server,
    context: &Value,
    first_id: &Value,
    second_id: &Value,
) -> Result<(Value, Value)> {
    let unrelated = server
        .blocking("alias", chat("another-context", None), 3)
        .await?;
    let other_export = server
        .blocking("runner", chat("other-export", None), 4)
        .await?;
    assert_ne!(unrelated["contextId"], *context);
    let list = server
        .rpc(RpcCall::new(
            "alias",
            "alice",
            "ListTasks",
            json!({"contextId":context}),
        ))
        .await?;
    let mut ids: Vec<_> = list["result"]["tasks"]
        .as_array()
        .context("ListTasks missing tasks")?
        .iter()
        .map(|task| {
            assert_eq!(&task["contextId"], context);
            task["id"].as_str().unwrap().to_owned()
        })
        .collect();
    ids.sort();
    let mut expected = vec![
        first_id.as_str().unwrap().to_owned(),
        second_id.as_str().unwrap().to_owned(),
    ];
    expected.sort();
    assert_eq!(ids, expected);
    Ok((unrelated, other_export))
}

async fn assert_cli_delete_and_isolation(
    server: &Server,
    context: &Value,
    tasks: [&Value; 2],
) -> Result<()> {
    let keys = server.keys(context).await?;
    assert_eq!(
        keys.iter()
            .filter(|key| key.contains("/a2a/tasks/"))
            .count(),
        2
    );
    assert_eq!(
        keys.iter()
            .filter(|key| key.contains("/a2a/messages/"))
            .count(),
        2
    );
    server.delete(context).await?;
    assert!(
        server.keys(context).await?.is_empty(),
        "CLI left session keys behind"
    );
    for task in tasks {
        let missing = server
            .rpc(RpcCall::new(
                "alias",
                "alice",
                "GetTask",
                json!({"id":task["id"]}),
            ))
            .await?;
        assert_eq!(missing["error"]["code"], -32001);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_discovery_jira_continuity_identity_dedupe_list_and_cli_delete() -> Result<()> {
    let server = Server::start().await?;
    discovery(&server).await?;
    assert_unauthorized_list_tasks(&server).await?;

    let fixture: Value = serde_json::from_str(include_str!("../fixtures/jira/assignment.json"))?;
    let assignment = fixture["params"]["message"].clone();
    let (first, rendered_data) = execute_jira_assignment_turn(&server, &assignment).await?;
    let context = &first["contextId"];

    assert_assignment_dedupe_and_rejected_resumes(&server, &first, &assignment).await?;

    let assignment_text = assignment["parts"][0]["text"].as_str().unwrap();
    let second =
        execute_context_follow_up_turn(&server, &first, &rendered_data, assignment_text).await?;

    let (unrelated, other_export) =
        assert_scoped_list_and_unrelated_tasks(&server, context, &first["id"], &second["id"])
            .await?;

    assert_cli_delete_and_isolation(&server, context, [&first, &second]).await?;

    server.completed("alias", &unrelated["id"]).await?;
    server.completed("runner", &other_export["id"]).await?;
    assert_eq!(server.h.llm.requests.lock().len(), 4);
    Ok(())
}

async fn stream_until_partial_and_disconnect(server: &Server, params: &Value) -> Result<Value> {
    let mut frames = server
        .stream("alias", "SendStreamingMessage", params.clone())
        .await?;
    let snapshot = frames.event().await?.context("missing initial task")?;
    let task = snapshot["task"].clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");
    let mut answer = snapshot_text(&task);
    until_artifact(&mut frames, &mut answer).await?;
    assert_eq!(answer, "Hello ");
    drop(frames);
    Ok(task)
}

async fn resubscribe_and_complete_stream(
    server: &Server,
    task: &Value,
    params: &Value,
) -> Result<()> {
    let mut resumed = server
        .stream("pkg%2Fagent", "SubscribeToTask", json!({"id":task["id"]}))
        .await?;
    let fresh = resumed
        .event()
        .await?
        .context("missing resubscribe snapshot")?;
    assert_eq!(fresh["task"]["id"], task["id"]);
    let mut answer = snapshot_text(&fresh["task"]);
    assert_eq!(answer, "Hello ");
    let mut retry = server
        .stream("pkg__agent", "SendStreamingMessage", params.clone())
        .await?;
    assert_eq!(retry.event().await?.unwrap()["task"]["id"], task["id"]);
    drop(retry);
    server.h.llm.release.notify_one();
    finish(&mut resumed, &mut answer).await?;
    assert_eq!(answer, "Hello world");
    let completed = server.completed("alias", &task["id"]).await?;
    assert_eq!(snapshot_text(&completed), "Hello world");
    Ok(())
}

async fn assert_terminal_stream_retry(server: &Server, task: &Value, params: Value) -> Result<()> {
    let mut terminal_retry = server
        .stream("alias", "SendStreamingMessage", params)
        .await?;
    let terminal = terminal_retry
        .event()
        .await?
        .context("missing terminal retry")?;
    assert_eq!(terminal["task"]["id"], task["id"]);
    assert_eq!(terminal["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(snapshot_text(&terminal["task"]), "Hello world");
    assert!(terminal_retry.event().await?.is_none());
    assert_eq!(server.h.llm.requests.lock().len(), 1);
    Ok(())
}

async fn execute_cancel_stream_scenario(server: &Server, prior_task: &Value) -> Result<()> {
    let mut cancel_stream = server
        .stream(
            "alias",
            "SendStreamingMessage",
            json!({"message":chat("cancel-follow-up", Some(&prior_task["contextId"]))}),
        )
        .await?;
    let cancel_task = cancel_stream
        .event()
        .await?
        .context("missing cancel task")?["task"]
        .clone();
    assert_eq!(cancel_task["contextId"], prior_task["contextId"]);
    assert_ne!(cancel_task["id"], prior_task["id"]);
    until_artifact(&mut cancel_stream, &mut String::new()).await?;
    let canceled = server
        .rpc(RpcCall::new(
            "pkg__agent",
            "alice",
            "CancelTask",
            json!({"id":cancel_task["id"]}),
        ))
        .await?;
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    let mut terminal = false;
    while let Some(event) = cancel_stream.event().await? {
        assert!(!terminal, "event after cancel terminal status");
        if event["statusUpdate"].is_object() {
            assert_eq!(
                event["statusUpdate"]["status"]["state"],
                "TASK_STATE_CANCELED"
            );
            terminal = true;
        }
    }
    assert!(terminal, "stream closed without canceled status");
    let persisted = server
        .rpc(RpcCall::new(
            "alias",
            "alice",
            "GetTask",
            json!({"id":cancel_task["id"]}),
        ))
        .await?;
    assert_eq!(
        persisted["result"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );
    assert_eq!(server.h.llm.requests.lock().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_stream_disconnect_resubscribe_dedupe_and_cancel() -> Result<()> {
    let server = Server::start().await?;
    let params = json!({"message":chat("stream-disconnect", None)});
    let task = stream_until_partial_and_disconnect(&server, &params).await?;
    resubscribe_and_complete_stream(&server, &task, &params).await?;
    assert_terminal_stream_retry(&server, &task, params).await?;
    execute_cancel_stream_scenario(&server, &task).await?;
    Ok(())
}
