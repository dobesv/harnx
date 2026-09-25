//! In-memory ACP agent/client transport used by integration tests.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    LoadSessionRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification,
};
use anyhow::{Context, Result};
use harnx_acp_server::permission::{ALLOW_OPTION_ID, REJECT_OPTION_ID};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::TEST_TIMEOUT;

struct BackgroundTasks(Vec<tokio::task::JoinHandle<()>>);

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum PermissionReply {
    Allow,
    Reject,
    Pending,
}

pub(crate) struct TestClient {
    pub(crate) connection: acp::ConnectionTo<acp::Agent>,
    pub(crate) notifications: mpsc::UnboundedReceiver<SessionNotification>,
    pub(crate) permissions: mpsc::UnboundedReceiver<RequestPermissionRequest>,
    _tasks: BackgroundTasks,
}
fn spawn_test_agent_transport(
    agent: Arc<harnx_acp_server::HarnxAgent>,
    stream: tokio::io::DuplexStream,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<harnx_acp_server::AcpConnection>,
) {
    let (read, write) = tokio::io::split(stream);
    let transport = acp::ByteStreams::new(write.compat_write(), read.compat());
    let (connection_tx, connection_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let handler_agent = Arc::clone(&agent);
        let _ = acp::Agent
            .builder()
            .on_receive_request_from(
                acp::Client,
                async move |request: LoadSessionRequest, responder, connection| {
                    handler_agent.set_connection(connection.clone()).await;
                    responder.respond(handler_agent.load_session(request).await?)
                },
                acp::on_receive_request!(),
            )
            .connect_with(transport, async move |connection| {
                let _ = connection_tx.send(connection);
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
    });
    (task, connection_rx)
}

fn spawn_test_client_transport(
    stream: tokio::io::DuplexStream,
    permission_reply: PermissionReply,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<acp::ConnectionTo<acp::Agent>>,
    mpsc::UnboundedReceiver<SessionNotification>,
    mpsc::UnboundedReceiver<RequestPermissionRequest>,
) {
    let (read, write) = tokio::io::split(stream);
    let transport = acp::ByteStreams::new(write.compat_write(), read.compat());
    let (notification_tx, notification_rx) = mpsc::unbounded_channel();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel();
    let (connection_tx, connection_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = acp::Client
            .builder()
            .on_receive_notification_from(
                acp::Agent,
                async move |notification: SessionNotification, _connection| {
                    let _ = notification_tx.send(notification);
                    Ok(())
                },
                acp::on_receive_notification!(),
            )
            .on_receive_request_from(
                acp::Agent,
                async move |request: RequestPermissionRequest, responder, _connection| {
                    let _ = permission_tx.send(request.clone());
                    let outcome = permission_outcome(&request, permission_reply).await;
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                acp::on_receive_request!(),
            )
            .connect_with(transport, async move |connection| {
                let _ = connection_tx.send(connection);
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
    });
    (task, connection_rx, notification_rx, permission_rx)
}

pub(crate) async fn attach_test_client(
    agent: Arc<harnx_acp_server::HarnxAgent>,
    permission_reply: PermissionReply,
) -> Result<TestClient> {
    let (agent_stream, client_stream) = tokio::io::duplex(64 * 1024);
    let (agent_task, agent_connection_rx) =
        spawn_test_agent_transport(Arc::clone(&agent), agent_stream);
    let (client_task, client_connection_rx, notifications, permissions) =
        spawn_test_client_transport(client_stream, permission_reply);

    let agent_connection = tokio::time::timeout(TEST_TIMEOUT, agent_connection_rx)
        .await
        .context("ACP test transport did not connect")?
        .context("ACP agent transport closed during setup")?;
    agent.set_connection(agent_connection).await;
    let client_connection = tokio::time::timeout(TEST_TIMEOUT, client_connection_rx)
        .await
        .context("ACP client transport did not connect")?
        .context("ACP client transport closed during setup")?;
    Ok(TestClient {
        connection: client_connection,
        notifications,
        permissions,
        _tasks: BackgroundTasks(vec![agent_task, client_task]),
    })
}

async fn permission_outcome(
    request: &RequestPermissionRequest,
    reply: PermissionReply,
) -> RequestPermissionOutcome {
    let option_id = match reply {
        PermissionReply::Allow => ALLOW_OPTION_ID,
        PermissionReply::Reject => REJECT_OPTION_ID,
        PermissionReply::Pending => return std::future::pending().await,
    };
    let option = request
        .options
        .iter()
        .find(|option| option.option_id.0.as_ref() == option_id)
        .expect("expected permission option");
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
}
