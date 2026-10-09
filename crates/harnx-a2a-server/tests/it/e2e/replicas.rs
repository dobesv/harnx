//! Two CLI processes, one isolated broker/worker, no frontend-local test controls.
use super::*;
use crate::support::alice;
use harnx_a2a_server::store::context::{AuthorityError, ContextWrite};
use harnx_core::{message::MessageRole, session::SessionLogEntry};
use harnx_runtime::nats_session_log::NatsSessionLog;

mod compatibility;
mod lifecycle;
mod recovery;

struct Pair {
    a: Server,
    b: Server,
}
impl Pair {
    async fn start(script: Script) -> Result<Self> {
        let a = Server::start_script(script).await?;
        let b = Server::start_replica(a.h.clone(), "a2a-b.log").await?;
        assert_ne!(a._process.child.id(), b._process.child.id());
        assert_ne!(a.base, b.base);
        Ok(Self { a, b })
    }
    fn routes(&self) -> Alternating<'_> {
        Alternating {
            pair: self,
            trace: vec![],
        }
    }
}

struct Alternating<'a> {
    pair: &'a Pair,
    trace: Vec<(usize, String)>,
}
impl Alternating<'_> {
    fn request(&mut self, call: RpcCall<'_>) -> reqwest::RequestBuilder {
        let index = self.trace.len() % 2;
        self.trace.push((index, call.method.into()));
        let server = if index == 0 {
            &self.pair.a
        } else {
            &self.pair.b
        };
        server.request(call)
    }
    async fn rpc(&mut self, call: RpcCall<'_>) -> Result<Value> {
        let response = self.request(call).send().await?;
        assert_eq!(response.status(), 200);
        let envelope: Value = response.json().await?;
        assert_eq!(envelope["jsonrpc"], "2.0");
        Ok(envelope)
    }
    async fn stream(&mut self, method: &str, params: Value) -> Result<Frames> {
        Frames::from_response(
            self.request(RpcCall::new("runner", "alice", method, params))
                .send()
                .await?,
        )
    }
    fn assert_routes(&self, methods: &[&str]) {
        for (position, (index, _)) in self.trace.iter().enumerate() {
            assert_eq!(*index, position % 2);
        }
        for method in methods {
            for index in 0..2 {
                assert!(
                    self.trace.iter().any(|(i, m)| *i == index && m == method),
                    "{method} missing from backend {index}: {:?}",
                    self.trace
                );
            }
        }
        eprintln!("alternating HTTP routes: {:?}", self.trace);
    }
}

fn immediate(message: Value) -> Value {
    json!({"message":message,"configuration":{"returnImmediately":true}})
}
fn task(envelope: &Value) -> Result<Value> {
    anyhow::ensure!(envelope["error"].is_null(), "{envelope}");
    envelope["result"]
        .get("task")
        .cloned()
        .context("task response")
}
fn tool_count(h: &Harness) -> Result<usize> {
    let text = std::fs::read_to_string(h.config_dir().join("executions")).with_context(|| {
        format!(
            "missing tool side effect; requests={:?}; logs={}",
            h.llm.requests.lock(),
            h.logs.text()
        )
    })?;
    assert!(text.lines().all(|line| line == "execution"));
    Ok(text.lines().count())
}
async fn one_prompt(h: &Harness, context: &Value) -> Result<()> {
    let storage =
        harnx_core::session_identity::session_key(Some("runner"), context.as_str().unwrap());
    let events = NatsSessionLog::new_with_replicas(h.jetstream.clone(), &storage, 1)
        .load_events_async()
        .await?;
    assert_eq!(
        events
            .iter()
            .filter(|(_, e)| matches!(
                e,
                SessionLogEntry::Message {
                    role: MessageRole::User,
                    ..
                }
            ))
            .count(),
        1
    );
    Ok(())
}
async fn settled(h: &Harness, id: &Value) -> Result<harnx_a2a_server::store::TaskRecord> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let record = h.task(id.as_str().context("task ID")?).await?;
            if record.task.status.state.is_terminal() {
                return Ok(record);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("background terminal deadline")?
}
async fn stale_ticket(h: &Harness, context: &Value) -> Result<(String, ContextWrite)> {
    let storage =
        harnx_core::session_identity::session_key(Some("runner"), context.as_str().unwrap());
    let authority = h
        .store
        .read_context(&storage)
        .await?
        .context("active authority")?;
    let write = h
        .store
        .prepare_context_update(&storage, &authority.version()?, "paused-old-owner", |_| {})
        .await?;
    Ok((storage, write))
}
async fn terminal_frames(frames: &mut Frames, state: &str) -> Result<()> {
    let mut terminal = false;
    while let Some(event) = frames.event().await? {
        assert!(!terminal, "event after terminal");
        if event["statusUpdate"].is_object() {
            assert_eq!(event["statusUpdate"]["status"]["state"], state);
            terminal = true;
        }
    }
    assert!(terminal, "stream closed without terminal event");
    Ok(())
}
