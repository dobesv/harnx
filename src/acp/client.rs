use super::AcpServerConfig;

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{anyhow, Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::thread;
use tokio::io::{
    AsyncBufReadExt, AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, BufReader, ReadBuf,
};
use tokio::process::{Child, Command};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::LocalSet;

pub struct AcpClient {
    name: String,
    config: AcpServerConfig,
    connected: Arc<RwLock<bool>>,
    connection_failed: Arc<RwLock<bool>>,
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    worker: Arc<Mutex<Option<AcpWorkerHandle>>>,
}

#[derive(Debug, Clone, Default)]
struct SessionState {
    response_text: String,
    stop_reason: Option<String>,
}

struct AcpWorkerHandle {
    tx: mpsc::UnboundedSender<WorkerCommand>,
    join: thread::JoinHandle<()>,
}

enum WorkerCommand {
    NewSession {
        respond_to: oneshot::Sender<Result<String>>,
    },
    Prompt {
        session_id: String,
        message: String,
        respond_to: oneshot::Sender<Result<String>>,
    },
    LoadSession {
        session_id: String,
        respond_to: oneshot::Sender<Result<()>>,
    },
    CancelSession {
        session_id: String,
        respond_to: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        respond_to: oneshot::Sender<Result<()>>,
    },
}

struct AcpNotificationClient {
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
}

struct TokioCompat<T> {
    inner: T,
}

impl AcpNotificationClient {
    fn new(sessions: Arc<RwLock<HashMap<String, SessionState>>>) -> Self {
        Self { sessions }
    }
}

impl<T> TokioCompat<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: TokioAsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: TokioAsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpNotificationClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        if let acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) =
            args.update
        {
            let chunk = content_block_to_text(&content);
            if !chunk.is_empty() {
                let session_id = args.session_id.0.to_string();
                let mut sessions = self.sessions.write().await;
                let state = sessions.entry(session_id).or_default();
                state.response_text.push_str(&chunk);
            }
        }

        Ok(())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(
        &self,
        _args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> acp::Result<()> {
        Err(acp::Error::method_not_found())
    }
}

impl AcpClient {
    pub fn new(config: AcpServerConfig) -> Self {
        let name = config.name.clone();
        Self {
            name,
            config,
            connected: Arc::new(RwLock::new(false)),
            connection_failed: Arc::new(RwLock::new(false)),
            initialize_response: Arc::new(RwLock::new(None)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            worker: Arc::new(Mutex::new(None)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn connect(&self) -> Result<()> {
        *self.connection_failed.write().await = false;
        if *self.connected.read().await {
            return Ok(());
        }

        let mut worker_guard = self.worker.lock().await;
        if worker_guard.is_some() {
            *self.connected.write().await = true;
            return Ok(());
        }

        let (worker, ready_rx) = spawn_worker(
            self.name.clone(),
            self.config.clone(),
            self.sessions.clone(),
            self.initialize_response.clone(),
        )?;

        match ready_rx.await {
            Ok(Ok(())) => {
                *self.connected.write().await = true;
                *worker_guard = Some(worker);
                Ok(())
            }
            Ok(Err(err)) => {
                *self.connection_failed.write().await = true;
                join_worker(worker.join).await;
                Err(err)
            }
            Err(_) => {
                *self.connection_failed.write().await = true;
                join_worker(worker.join).await;
                Err(anyhow!(
                    "ACP server '{}' stopped during initialization",
                    self.name
                ))
            }
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        let worker = self.worker.lock().await.take();

        *self.connected.write().await = false;
        *self.connection_failed.write().await = false;
        *self.initialize_response.write().await = None;
        self.sessions.write().await.clear();

        if let Some(worker) = worker {
            let (respond_to, response_rx) = oneshot::channel();
            let _ = worker.tx.send(WorkerCommand::Shutdown { respond_to });
            let shutdown_result = match response_rx.await {
                Ok(result) => result,
                Err(_) => Ok(()),
            };
            join_worker(worker.join).await;
            shutdown_result?;
        }

        Ok(())
    }

    pub async fn session_new(&self) -> Result<String> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::NewSession { respond_to })
            .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        response_rx
            .await
            .map_err(|_| anyhow!("ACP server '{}' disconnected during session/new", self.name))?
    }

    pub async fn session_prompt(&self, session_id: Option<&str>, message: &str) -> Result<String> {
        self.ensure_connected().await?;

        let session_id = match session_id {
            Some(session_id) => session_id.to_owned(),
            None => self.session_new().await?,
        };

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::Prompt {
            session_id,
            message: message.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        response_rx.await.map_err(|_| {
            anyhow!(
                "ACP server '{}' disconnected during session/prompt",
                self.name
            )
        })?
    }

    pub async fn session_load(&self, session_id: &str) -> Result<()> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::LoadSession {
            session_id: session_id.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        response_rx.await.map_err(|_| {
            anyhow!(
                "ACP server '{}' disconnected during session/load",
                self.name
            )
        })?
    }

    pub async fn session_cancel(&self, session_id: &str) -> Result<()> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::CancelSession {
            session_id: session_id.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        response_rx.await.map_err(|_| {
            anyhow!(
                "ACP server '{}' disconnected during session/cancel",
                self.name
            )
        })?
    }

    async fn ensure_connected(&self) -> Result<()> {
        if !*self.connected.read().await {
            self.connect().await?;
        }
        Ok(())
    }

    async fn worker_sender(&self) -> Result<mpsc::UnboundedSender<WorkerCommand>> {
        self.worker
            .lock()
            .await
            .as_ref()
            .map(|worker| worker.tx.clone())
            .ok_or_else(|| anyhow!("ACP server '{}' is not connected", self.name))
    }
}

fn spawn_worker(
    name: String,
    config: AcpServerConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
) -> Result<(AcpWorkerHandle, oneshot::Receiver<Result<()>>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let thread_name = format!("acp-client-{name}");
    let config_name = config.name.clone();

    let join = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(anyhow!(
                        "Failed to create ACP runtime for '{}': {err}",
                        name
                    )));
                    return;
                }
            };

            let local_set = LocalSet::new();
            let result = local_set.block_on(&runtime, async move {
                worker_main(name, config, sessions, initialize_response, rx, ready_tx).await
            });

            if let Err(err) = result {
                log::warn!("ACP worker exited with error: {err}");
            }
        })
        .with_context(|| format!("Failed to start ACP worker thread for '{}'", config_name))?;

    Ok((AcpWorkerHandle { tx, join }, ready_rx))
}

async fn worker_main(
    name: String,
    config: AcpServerConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
    mut rx: mpsc::UnboundedReceiver<WorkerCommand>,
    ready_tx: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&config.env)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Failed to spawn ACP server '{}'", name))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ACP server '{}' did not provide stdout", name))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ACP server '{}' did not provide stdin", name))?;

    if let Some(stderr) = child.stderr.take() {
        let server_name = name.clone();
        tokio::task::spawn_local(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[acp:{}] {}", server_name, line);
            }
        });
    }

    let client = AcpNotificationClient::new(sessions.clone());
    let (conn, handle_io) = acp::ClientSideConnection::new(
        client,
        TokioCompat::new(stdin),
        TokioCompat::new(stdout),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    let conn = Rc::new(conn);

    tokio::task::spawn_local(async move {
        if let Err(err) = handle_io.await {
            log::debug!("ACP I/O loop exited: {err}");
        }
    });

    let init = conn
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                acp::Implementation::new("harnx", env!("CARGO_PKG_VERSION")).title("Harnx"),
            ),
        )
        .await
        .with_context(|| format!("Failed to initialize ACP server '{}'", name))?;

    *initialize_response.write().await = Some(init);
    let _ = ready_tx.send(Ok(()));

    let child = Rc::new(RefCell::new(Some(child)));

    while let Some(command) = rx.recv().await {
        match command {
            WorkerCommand::NewSession { respond_to } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        let response = conn
                            .new_session(acp::NewSessionRequest::new(std::env::current_dir()?))
                            .await
                            .with_context(|| {
                                format!("Failed to create ACP session on '{}'", server_name)
                            })?;
                        let session_id = response.session_id.0.to_string();
                        sessions
                            .write()
                            .await
                            .insert(session_id.clone(), SessionState::default());
                        Ok(session_id)
                    }
                    .await;
                    let _ = respond_to.send(result);
                });
            }
            WorkerCommand::Prompt {
                session_id,
                message,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        {
                            let mut sessions = sessions.write().await;
                            let state = sessions.entry(session_id.clone()).or_default();
                            state.response_text.clear();
                            state.stop_reason = None;
                        }

                        let response = conn
                            .prompt(acp::PromptRequest::new(
                                session_id.clone(),
                                vec![message.into()],
                            ))
                            .await
                            .with_context(|| {
                                format!(
                                    "Failed to send ACP prompt to session '{}' on '{}'",
                                    session_id, server_name
                                )
                            })?;

                        let mut sessions = sessions.write().await;
                        let state = sessions.entry(session_id.clone()).or_default();
                        state.stop_reason = Some(format!("{:?}", response.stop_reason));
                        Ok(state.response_text.clone())
                    }
                    .await;
                    let _ = respond_to.send(result);
                });
            }
            WorkerCommand::LoadSession {
                session_id,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        conn.load_session(acp::LoadSessionRequest::new(
                            session_id.clone(),
                            std::env::current_dir()?,
                        ))
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to load ACP session '{}' on '{}'",
                                session_id, server_name
                            )
                        })?;

                        sessions.write().await.entry(session_id).or_default();
                        Ok(())
                    }
                    .await;
                    let _ = respond_to.send(result);
                });
            }
            WorkerCommand::CancelSession {
                session_id,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = conn
                        .cancel(acp::CancelNotification::new(session_id.clone()))
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to cancel ACP session '{}' on '{}'",
                                session_id, server_name
                            )
                        });
                    let _ = respond_to.send(result);
                });
            }
            WorkerCommand::Shutdown { respond_to } => {
                let result = shutdown_child(&child).await;
                let _ = respond_to.send(result);
                break;
            }
        }
    }

    Ok(())
}

async fn shutdown_child(child: &Rc<RefCell<Option<Child>>>) -> Result<()> {
    let child = child.borrow_mut().take();
    if let Some(mut child) = child {
        if let Err(err) = child.kill().await {
            if err.kind() != std::io::ErrorKind::InvalidInput {
                return Err(err).context("Failed to kill ACP subprocess");
            }
        }
        let _ = child.wait().await;
    }
    Ok(())
}

async fn join_worker(join: thread::JoinHandle<()>) {
    let _ = tokio::task::spawn_blocking(move || {
        let _ = join.join();
    })
    .await;
}

fn content_block_to_text(content: &acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.to_string(),
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::Resource(_) => "<resource>".to_string(),
        _ => String::new(),
    }
}
