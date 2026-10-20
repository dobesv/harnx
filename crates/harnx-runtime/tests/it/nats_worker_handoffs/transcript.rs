//! Dump and model-request coverage from a real worker's persisted parent transcript.

use super::*;
use anyhow::{ensure, Context};
use harnx_client::{
    claude::claude_build_chat_completions_body, openai::openai_build_chat_completions_body,
    ChatCompletionsData, Model,
};
use harnx_core::{agent_config::AgentConfig, input::Input, message::Message};
use harnx_runtime::config::{input, load_session_for_render};

const PARENT_SESSION_ID: &str = "nats-handoff-generated-root";

async fn parent_entries(fixture: &HandoffFixture) -> Result<Vec<(u64, SessionLogEntry)>> {
    NatsSessionLog::for_agent(fixture.jetstream.clone(), "source-agent", PARENT_SESSION_ID)
        .load_events_async()
        .await
}

fn committed_target(entries: &[(u64, SessionLogEntry)]) -> (&str, &str) {
    entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::HandoffCommitted {
                target_agent,
                target_session_id,
                ..
            } => Some((target_agent.as_str(), target_session_id.as_str())),
            _ => None,
        })
        .expect("parent has a durable handoff commit")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_handoff_text_dump_prints_target_once() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    fixture.run_generated_scenario().await?;
    let entries = parent_entries(&fixture).await?;
    let (agent, session_id) = committed_target(&entries);
    let expected = format!("Handing off to {agent}; target session: {session_id}.");

    let binary = std::env::current_exe()?
        .parent()
        .context("test executable directory")?
        .parent()
        .context("workspace binary directory")?
        .join(format!("harnx{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        binary.is_file(),
        "build harnx first: {} is missing",
        binary.display()
    );
    let mut command = tokio::process::Command::new(binary);
    command
        .args([
            "dump",
            "session",
            "source-agent@local",
            PARENT_SESSION_ID,
            "--format",
            "text",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(CI_SAFE_TIMEOUT, command.output()).await??;
    // Tool results go to stderr; model text goes to stdout in the CLI sink.
    let text = format!(
        "{}{}",
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?,
    );
    assert!(output.status.success(), "text dump failed: {text}");
    assert_eq!(text.matches(&expected).count(), 1, "{text}");
    assert_eq!(text.matches(agent).count(), 1, "{text}");
    assert_eq!(text.matches(session_id).count(), 1, "{text}");
    assert!(!text.contains("HandoffCommitted"), "{text}");
    assert!(!text.contains("handoff_committed"), "{text}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_handoff_model_requests_include_persisted_child_session() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    fixture.run_generated_scenario().await?;
    let entries = parent_entries(&fixture).await?;
    let (agent, session_id) = committed_target(&entries);
    let config = fixture.config.read().clone();
    let session =
        load_session_for_render(&config, Some("local"), PARENT_SESSION_ID, "source-agent").await?;
    let parent_config = Arc::new(ConfigLock::new(Config {
        session: Some(session),
        ..config
    }));
    let mut input = Input::new(
        "generated handoff".into(),
        ("generated handoff".into(), vec![]),
        AgentConfig::from_prompt("Source instructions"),
    );
    // Build requests only. Handoff must not trigger another parent model round.
    input.with_session = true;
    input.skip_user_log_append = true;
    let messages = input::build_messages(&input, &parent_config)?;
    let results: Vec<_> = messages
        .iter()
        .flat_map(|message| match &message.content {
            MessageContent::ToolCalls(calls) => calls.tool_results.iter().collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(results.len(), 1, "one reconstructed handoff result");
    assert_eq!(
        results[0].output["handoff"],
        json!({"agent": agent, "session_id": session_id})
    );
    let expected_output = results[0].output.clone();
    assert_provider_handoff_requests(&messages, expected_output, session_id)?;
    assert_eq!(
        parent_entries(&fixture).await?,
        entries,
        "request building must not append a parent turn"
    );
    Ok(())
}

fn assert_provider_handoff_requests(
    messages: &[Message],
    expected_output: serde_json::Value,
    session_id: &str,
) -> Result<()> {
    let data = || ChatCompletionsData {
        messages: messages.to_vec(),
        temperature: None,
        top_p: None,
        functions: None,
        stream: false,
        attachments_dir: None,
    };

    let claude = claude_build_chat_completions_body(
        data(),
        &Model::new("claude", "claude-3-5-sonnet"),
        &Default::default(),
    )?;
    let claude_result = claude["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|part| part["type"] == "tool_result")
        .expect("Claude request has a tool result");
    let claude_output: serde_json::Value = serde_json::from_str(
        claude_result["content"]
            .as_str()
            .expect("Claude serializes tool output as text"),
    )?;
    assert_eq!(claude_output, expected_output);
    assert!(serde_json::to_string(&claude)?.contains(session_id));

    let openai = openai_build_chat_completions_body(data(), &Model::new("openai", "gpt-4o"));
    let openai_result = openai["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("OpenAI request has a tool result");
    let openai_output: serde_json::Value = serde_json::from_str(
        openai_result["content"]
            .as_str()
            .expect("OpenAI serializes tool output as text"),
    )?;
    assert_eq!(openai_output, expected_output);
    assert!(serde_json::to_string(&openai)?.contains(session_id));
    Ok(())
}
