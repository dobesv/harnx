use super::{run_bash_script, stdout_block, ConfigLock, ServerScope};
use anyhow::{Context, Result};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{Session, SessionLogEntry};
use harnx_runtime::config::{session::SessionAppendSink, AgentConfig, NatsRouting};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::nats_worker::NatsSessionLogBackend;
use harnx_runtime::session_history::TOOL_NAME as HISTORY_TOOL;
use harnx_runtime::session_meta_tool::{READ_TOOL_NAME, WRITE_TOOL_NAME};
use serde_json::json;
use std::sync::Arc;

const SESSION_ID: &str = "tools1";
const TOOLS: [&str; 3] = [HISTORY_TOOL, READ_TOOL_NAME, WRITE_TOOL_NAME];

struct ScriptSession {
    config: Arc<ConfigLock>,
    scope: ServerScope,
    _broker: crate::common::NatsServerHandle,
}

impl ScriptSession {
    async fn start() -> Result<Self> {
        let broker = crate::worker::require_nats_server()
            .await?
            .context("nats-server required for session tool scripts")?;
        let config = crate::worker::local_nats_runtime_config(broker.url());
        let session = seed_session(broker.url()).await?;
        let mut agent = AgentConfig::from_prompt("test");
        agent.set_bash_tools(Some(TOOLS.map(str::to_string).to_vec()));
        {
            let mut guard = config.write();
            guard.nats_routing = NatsRouting::Cluster("local".into());
            guard.agent = Some(harnx_runtime::config::Agent::new(agent));
            guard.session = Some(session);
        }
        Ok(Self {
            config,
            scope: ServerScope::new(),
            _broker: broker,
        })
    }

    async fn stdout(&self, script: &str) -> Result<String> {
        let output = run_bash_script(&self.config, &self.scope, script).await?;
        assert_eq!(output["status"], "ok", "{output}");
        Ok(stdout_block(&output))
    }
}

async fn seed_session(url: &str) -> Result<Session> {
    let js = async_nats::jetstream::new(async_nats::connect(url).await?);
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let metadata = SessionMetadata::new(
        SESSION_ID,
        SessionInitializer::inline("test", Default::default(), Default::default()),
    );
    store
        .create(&metadata)
        .await?
        .context("create script session")?;
    let key = metadata.storage_key();
    NatsSessionLog::new_with_replicas(js.clone(), key.clone(), 1)
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("seeded message".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let sink: Arc<dyn SessionAppendSink> =
        Arc::new(NatsSessionLogBackend::new(js, key, 1).with_metadata_store(Some(store)));
    Ok(Session {
        id: SESSION_ID.into(),
        runtime: Some(Arc::new(sink)),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_session_tools_pipe_payloads_in_all_output_modes() -> Result<()> {
    let session = ScriptSession::start().await?;
    let stdout = session
        .stdout(
            r#"set -o pipefail
harnx_agent_session_history_read | jq -r '.[0].text'
harnx_agent_session_history_read -o text | jq -r '.[0].role'
harnx_agent_session_history_read -o json | jq -r '.[0].type'
harnx_agent_session_history_read --jaq '.[0].text' -o json | jq -r .
harnx_agent_session_history_read --jaq null -o json
harnx_read_session_meta | jq -r .session_id
harnx_read_session_meta -o text | jq -r .session_id
harnx_read_session_meta -o json | jq -r .session_id
harnx_write_session_meta --add-labels default | jq -r '.properties.labels.value | join(",")'
harnx_write_session_meta --add-labels text -o text | jq -r '.properties.labels.value | join(",")'
harnx_write_session_meta --add-labels json -o json | jq -r '.properties.labels.value | join(",")'
harnx_read_session_meta | jq -r '.properties.labels.value | join(",")'"#,
        )
        .await?;
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "seeded message",
            "user",
            "message",
            "seeded message",
            "null",
            SESSION_ID,
            SESSION_ID,
            SESSION_ID,
            "default",
            "default,text",
            "default,text,json",
            "default,text,json"
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_python_session_tools_return_lists_and_dicts() -> Result<()> {
    let session = ScriptSession::start().await?;
    let stdout = session
        .stdout(
            r#"python3 - <<'PY'
rows = tools.harnx_agent_session_history_read()
assert isinstance(rows, list)
assert rows[0]['text'] == 'seeded message'
assert tools.harnx_agent_session_history_read(jaq='length') == 1
assert tools.harnx_agent_session_history_read(jaq='null') is None
written = tools.harnx_write_session_meta(add_labels=['python'])
assert isinstance(written, dict)
assert written['properties']['labels']['value'] == ['python']
read = tools.harnx_read_session_meta()
assert isinstance(read, dict)
assert read == written
print(read['session_id'])
PY"#,
        )
        .await?;
    assert_eq!(stdout, SESSION_ID);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_help_describes_session_tool_results() -> Result<()> {
    let session = ScriptSession::start().await?;
    for tool in [READ_TOOL_NAME, WRITE_TOOL_NAME] {
        let stdout = session.stdout(&format!("help {tool}")).await?;
        let (before_fields, fields) = stdout
            .split_once("\nFIELDS (-o json)\n")
            .context("meta help fields")?;
        assert!(before_fields.contains("\nOUTPUT\n"), "{stdout}");
        for (name, kind) in [
            ("session_id", "string"),
            ("agent", "string"),
            ("title", "string"),
            ("created_at", "string"),
            ("properties", "object"),
            ("observed_repositories", "array of object"),
        ] {
            assert!(
                fields.lines().any(|line| line
                    .trim_start()
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.trim_start().starts_with(kind))),
                "{name} ({kind}) missing from {stdout}"
            );
        }
    }
    let stdout = session.stdout(&format!("help {HISTORY_TOOL}")).await?;
    let (_, output) = stdout
        .split_once("\nOUTPUT\n")
        .context("history help output")?;
    assert!(
        output.contains("JSON array") && output.contains("--jaq"),
        "{stdout}"
    );
    assert!(!output.contains("FIELDS (-o json)"), "{stdout}");
    Ok(())
}

#[test]
fn model_declarations_keep_session_script_shapes_private() -> Result<()> {
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_use_tools(Some(TOOLS.map(str::to_string).to_vec()));
    let config = harnx_runtime::config::Config::default();
    let declarations = config
        .select_tools(&agent)
        .context("session tool declarations")?;
    assert_eq!(declarations.len(), TOOLS.len());
    for declaration in declarations {
        let model = serde_json::to_value(&declaration)?;
        assert_eq!(
            model,
            json!({
                "name": declaration.name,
                "description": declaration.description,
                "parameters": declaration.parameters,
            })
        );
    }
    Ok(())
}
