use super::*;

use harnx_toolset::Toolset;
use harnx_toolset_server::content::audience;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, InitializeRequestParams};
use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
use std::path::Path;
use tokio::io::duplex;
use uuid::Uuid;

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harnx-fs-tools-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Default)]
struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder().build(),
            Implementation::new("test", "0.1"),
        )
    }
}

struct TestConnection {
    _server_service: RunningService<RoleServer, FsServer>,
    client_service: RunningService<RoleClient, TestClientHandler>,
}

async fn connect_server(server: FsServer, _roots: Vec<PathBuf>) -> TestConnection {
    let (client_transport, server_transport) = duplex(65_536);
    let server_fut = serve_server(server, server_transport);
    let client_fut = serve_client(TestClientHandler, client_transport);
    let (server_res, client_res) = tokio::join!(server_fut, client_fut);
    TestConnection {
        _server_service: server_res.unwrap(),
        client_service: client_res.unwrap(),
    }
}

fn text_content(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.clone()))
        .unwrap()
}

fn rwx_allowlist(paths: impl IntoIterator<Item = PathBuf>) -> ResolvedAllowlist {
    let mut allowlist = ResolvedAllowlist::new();
    for path in paths {
        allowlist.insert_rwx(path);
    }
    allowlist
}

fn make_server(dir: &Path) -> FsServer {
    FsServer::new(rwx_allowlist([dir.to_path_buf()]))
}

fn write_fixture(dir: &TestDir, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn fixture_path(dir: &TestDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed with {status}", args);
}

fn init_git_repo(dir: &Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.name", "harnx test"]);
    git(dir, &["config", "user.email", "harnx@example.com"]);
}

fn user_summary(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter(|content| {
            audience(content)
                .map(|a| a.contains(&Role::User))
                .unwrap_or(false)
        })
        .find_map(|content| content.as_text().map(|text| text.text.clone()))
        .unwrap_or_default()
}

fn tool_args(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

mod allowlist;
mod concurrency;
mod directory_search;
mod edit_ops;
#[cfg(unix)]
mod history_diff;
mod insert_ops;
mod protocol;
mod read_ops;
mod regex_replace;
