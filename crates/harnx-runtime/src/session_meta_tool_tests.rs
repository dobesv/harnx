use super::*;
use crate::config::session::SessionAppendSink;
use crate::config::{Config, ConfigLock};
use crate::nats_session_metadata::{SessionInitializer, SESSION_PROPERTIES_NAMESPACE};
use harnx_core::session::{Session, SessionLogEntry};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

type MetadataFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Option<SessionMetadata>>> + Send + 'a>>;

/// A sink holding one session's metadata in memory.
struct MemoryMetadataSink(Mutex<SessionMetadata>);

impl SessionAppendSink for MemoryMetadataSink {
    fn append(&self, _entry: &SessionLogEntry) -> anyhow::Result<u64> {
        anyhow::bail!("the session metadata tools never append to the log")
    }

    fn load_metadata(&self) -> MetadataFuture<'_> {
        let metadata = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(Some(metadata)) })
    }

    fn persist_session_properties<'a>(
        &'a self,
        update: &'a SessionPropertiesUpdate,
    ) -> MetadataFuture<'a> {
        Box::pin(async move {
            let mut metadata = self.0.lock().unwrap();
            let mut properties = session_properties(&metadata)?;
            update.apply(&mut properties)?;
            metadata.extensions.insert(
                SESSION_PROPERTIES_NAMESPACE.to_string(),
                serde_json::to_value(properties)?,
            );
            Ok(Some(metadata.clone()))
        })
    }
}

fn metadata() -> SessionMetadata {
    let mut metadata = SessionMetadata::new(
        "abc123",
        SessionInitializer::named("pantheon/atlas", Default::default()),
    );
    metadata.title.value = Some("Session metadata tools".to_string());
    metadata
}

fn provider(session: Option<Session>) -> SessionMetaProvider {
    let config = Config {
        session,
        ..Config::default()
    };
    SessionMetaProvider::new(Arc::new(ConfigLock::new(config)))
}

fn provider_for(metadata: SessionMetadata) -> SessionMetaProvider {
    let sink: Arc<dyn SessionAppendSink> = Arc::new(MemoryMetadataSink(Mutex::new(metadata)));
    provider(Some(Session {
        id: "abc123".to_string(),
        runtime: Some(Arc::new(sink)),
        ..Default::default()
    }))
}

/// The JSON view a call returns, or its error as `recoverable: ...` or
/// `fatal: ...`.
async fn call(provider: &SessionMetaProvider, tool: &str, args: Value) -> Result<Value, String> {
    let output = provider
        .call_tool(tool, args, &harnx_core::abort::create_abort_signal())
        .await
        .map_err(|error| match error {
            ToolError::Recoverable(error) => format!("recoverable: {error:#}"),
            ToolError::Fatal(error) => format!("fatal: {error:#}"),
        })?;
    let text = output.value["content"][0]["text"]
        .as_str()
        .expect("text content");
    Ok(serde_json::from_str(text).expect("JSON view"))
}

async fn call_error(provider: &SessionMetaProvider, tool: &str, args: Value) -> String {
    let error = call(provider, tool, args)
        .await
        .expect_err("the call must fail");
    assert!(error.starts_with("recoverable: "), "{error}");
    error
}

#[test]
fn declarations_are_generated_from_the_property_table() {
    let read = read_tool_declaration();
    assert_eq!(read.name, READ_TOOL_NAME);
    assert_eq!(read.kind, Some(ToolKind::Read));
    assert_eq!(read.read_only_hint, Some(true));
    assert!(read
        .parameters
        .properties()
        .is_some_and(|properties| properties.is_empty()));

    let write = write_tool_declaration();
    assert_eq!(write.name, WRITE_TOOL_NAME);
    assert_eq!(write.kind, Some(ToolKind::Edit));
    for definition in PROPERTY_DEFINITIONS {
        assert!(
            read.description.contains(definition.name)
                && write.description.contains(definition.name),
            "{} is missing from a description",
            definition.name
        );
    }
    let parameters = write.parameters.properties().expect("write parameters");
    let mut names: Vec<_> = parameters.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["add_labels", "clear", "remove_labels", "set"]);
    assert_eq!(
        write.parameters.as_value()["additionalProperties"],
        json!(false)
    );
}

#[tokio::test]
async fn read_returns_identity_properties_and_observed_repositories() {
    let mut metadata = metadata();
    metadata.extensions.insert(
        SESSION_PROPERTIES_NAMESPACE.to_string(),
        json!({"github_issue": {"value": 2296, "inherit": true}}),
    );
    metadata.extensions.insert(
        harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE.to_string(),
        json!({
            "version": 1,
            "contexts": [{
                "version": 1,
                "observed_at": "2026-10-03T00:00:00Z",
                "workspace_root": "/private/workspace",
                "working_directory": "/private/workspace/repo",
                "repository": {
                    "worktree_root": "/private/workspace/repo",
                    "branch": "feature",
                    "remotes": [{"name": "origin", "repository": "github.com/acme/repo", "primary": true}]
                },
                "provenance": {
                    "server_scope": "scope",
                    "server_identity": "fs",
                    "tool_name": "read",
                    "call_id": "call",
                    "worker_received_at": "2026-10-03T00:00:00Z"
                }
            }]
        }),
    );
    let created_at = metadata.created_at;
    let view = call(&provider_for(metadata), READ_TOOL_NAME, json!({}))
        .await
        .unwrap();
    assert_eq!(
        view,
        json!({
            "session_id": "abc123",
            "agent": "pantheon/atlas",
            "title": "Session metadata tools",
            "created_at": created_at,
            "properties": {"github_issue": {"value": 2296, "inherit": true}},
            "observed_repositories": [{"repository": "github.com/acme/repo", "branch": "feature"}],
        })
    );
    assert!(!view.to_string().contains("/private/workspace"));
}

#[tokio::test]
async fn write_applies_the_change_and_returns_the_new_view() {
    let provider = provider_for(metadata());
    let view = call(
        &provider,
        WRITE_TOOL_NAME,
        json!({
            "set": [
                {"name": "github_owner_repo", "value": "dobesv/harnx"},
                {"name": "github_pull_request", "value": "#2300"},
                {"name": "customer", "value": "acme", "inherit": true},
            ],
            "add_labels": ["in-review"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        view["properties"],
        json!({
            "github_owner_repo": {"value": "dobesv/harnx", "inherit": true},
            "github_pull_request": {"value": 2300, "inherit": true},
            "customer": {"value": "acme", "inherit": true},
            "labels": {"value": ["in-review"], "inherit": false},
        })
    );
    let read = call(&provider, READ_TOOL_NAME, json!({})).await.unwrap();
    assert_eq!(read["properties"], view["properties"]);
}

#[tokio::test]
async fn write_reports_bad_arguments_as_recoverable_errors() {
    let provider = provider_for(metadata());
    for (args, expected) in [
        (json!({"labels": ["a"]}), "unknown field `labels`"),
        (
            json!({"set": [{"name": "github_issue", "value": "soon"}]}),
            "github_issue must be a positive integer",
        ),
        (
            json!({"set": [{"name": "user_id", "value": "mallory"}]}),
            "user_id is set by Harnx",
        ),
    ] {
        let message = call_error(&provider, WRITE_TOOL_NAME, args).await;
        assert!(message.contains(expected), "{message}");
    }
    let read = call(&provider, READ_TOOL_NAME, json!({})).await.unwrap();
    assert_eq!(
        read["properties"],
        json!({}),
        "a rejected change writes nothing"
    );
}

#[tokio::test]
async fn sessions_without_canonical_metadata_fail_recoverably() {
    let message = call_error(&provider(None), READ_TOOL_NAME, json!({})).await;
    assert!(message.contains("no canonical metadata"), "{message}");

    // A sink that keeps no metadata, such as the in-memory log, answers None.
    let mut session = Session {
        id: "abc123".to_string(),
        ..Default::default()
    };
    crate::config::session::attach_memory_log(&mut session);
    let message = call_error(&provider(Some(session)), WRITE_TOOL_NAME, json!({})).await;
    assert!(message.contains("no canonical metadata"), "{message}");
}
