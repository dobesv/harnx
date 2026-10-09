//! Real HTTP/SSE across independent backends, with ordered handoff pause points.
use super::admission::{remove_owner_lease, reservation};
use super::*;
use crate::streaming::{snapshot_text, Frames};
use a2a_lf::{PartContent, TaskState};
use tokio_util::task::AbortOnDropHandle;

mod acknowledgements;
mod faults;
mod handoff;
mod lag;

struct RemoteHttp {
    url: String,
    client: reqwest::Client,
    _server: AbortOnDropHandle<()>,
}
impl RemoteHttp {
    async fn start(replica: &Replica) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/agents/runner", listener.local_addr()?);
        let app = replica.app.clone();
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Ok(Self {
            url,
            client: reqwest::Client::builder().timeout(DEADLINE).build()?,
            _server: server,
        })
    }
    async fn open(&self, method: &str, params: Value) -> Result<Frames> {
        Frames::from_response(
            self.client
                .post(&self.url)
                .header("X-User-ID", "alice")
                .json(
                    &json!({"jsonrpc":"2.0","id":"stream-request","method":method,"params":params}),
                )
                .send()
                .await?,
        )
    }
}
pub(super) async fn wait_cursor(t: &TwoBackends, cursor: u64, terminal: bool) -> Result<()> {
    let saved = reservation(t).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context =
                t.b.backend
                    .store
                    .read_context(&saved.allocation.storage_key)
                    .await?
                    .context("stream authority")?;
            let active = context
                .document
                .state
                .active
                .as_ref()
                .context("stream active")?;
            if active.publication.stream_seq >= cursor
                && active.publication.pending.is_none()
                && (!terminal || active.snapshot.task.status.state == TaskState::Completed)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
async fn finish_frames(frames: &mut Frames, first: Value) -> Result<String> {
    let mut text = snapshot_text(&first["task"]);
    let mut terminal = first["task"]["status"]["state"] == "TASK_STATE_COMPLETED";
    while let Some(frame) = frames.event().await? {
        if let Some(artifact) = frame.get("artifactUpdate") {
            let delta: String = artifact["artifact"]["parts"]
                .as_array()
                .context("artifact parts")?
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect();
            if artifact["append"] != true {
                text.clear();
            }
            text.push_str(&delta);
        }
        if let Some(status) = frame.get("statusUpdate") {
            terminal = status["status"]["state"] == "TASK_STATE_COMPLETED";
        }
    }
    assert!(terminal, "stream must include terminal snapshot or status");
    Ok(text)
}
async fn direct_finish(
    mut events: tokio::sync::broadcast::Receiver<harnx_a2a_server::runner::A2aEvent>,
    mut text: String,
) -> Result<String> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let event = events.recv().await?;
            if let a2a_lf::StreamResponse::ArtifactUpdate(update) = &event.response {
                if update.append != Some(true) {
                    text.clear();
                }
                text.extend(
                    update
                        .artifact
                        .parts
                        .iter()
                        .filter_map(|part| match &part.content {
                            PartContent::Text(text) => Some(text.as_str()),
                            _ => None,
                        }),
                );
            }
            if event.is_terminal() {
                return Ok(text);
            }
        }
    })
    .await?
}
