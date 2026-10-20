//! Provider-style handoff fixtures and dispatch assertions.

use super::*;

struct ProviderHandoffToolset;

#[async_trait::async_trait]
impl harnx_toolset::Toolset for ProviderHandoffToolset {
    fn name(&self) -> &str {
        "fixture"
    }

    fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
        vec![harnx_toolset::ToolSpec {
            name: "handoff".into(),
            description: "Return a provider-style handoff".into(),
            input_schema: json!({"type": "object"}),
            cancellation_guarantee: Default::default(),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        _: &str,
        args: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, harnx_toolset::ToolInvokeError> {
        Ok(args)
    }
}

pub(super) async fn start_tool_server(
    client: &async_nats::Client,
    jetstream: &async_nats::jetstream::Context,
    provider_env: &mut Vec<EnvVarGuard>,
) -> Result<tokio_util::task::AbortOnDropHandle<Result<()>>> {
    let scope = harnx_core::instance::ServerScope::new();
    let guard = EnvVarGuard::set("HARNX_SERVER_SCOPE", scope.as_str());
    let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        harnx_toolset_server::serve_with_client(
            Arc::new(ProviderHandoffToolset),
            scope.clone(),
            harnx_nats_common::connect::NatsConnection {
                client: client.clone(),
                replicas: 1,
            },
        ),
    ));
    let key = harnx_toolset_server::registration_key(
        &scope,
        &harnx_toolset::server_identity_token(None, "", "fixture"),
    );
    wait_for_registration(jetstream, &key).await?;
    provider_env.push(guard);
    Ok(server)
}

pub(super) async fn run_provider_handoff(explicit: bool) -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start_with_provider(true).await? else {
        return Ok(());
    };
    let source = fixture.session("source-agent", "provider-source").await?;
    let stream = fixture.source_stream(&source).await?;
    let prompt = if explicit {
        "provider explicit"
    } else {
        "provider generated"
    };
    source
        .clone()
        .with_external_admission()
        .run_turn(prompt, Arc::new(NullSink), None)
        .await?;
    let entries = fixture.log(&source).load_events_async().await?;
    let (target_agent, target_id) = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::HandoffCommitted {
                target_agent,
                target_session_id,
                handoff_tool_call_id,
            } => {
                assert_eq!(handoff_tool_call_id.as_deref(), Some("handoff-call-1"));
                Some((target_agent, target_session_id))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("provider result must dispatch a handoff: {entries:#?}"));
    if explicit {
        assert_eq!(target_id, EXPLICIT_TARGET_ID);
    } else {
        assert_generated_target_identity(target_agent, target_id);
    }
    assert_provider_output(&entries, explicit);
    let observed = observe_source_handoff(stream).await?;
    assert_eq!(
        observed.requested,
        Some((
            "delegate-agent".into(),
            explicit.then(|| EXPLICIT_TARGET_ID.to_string())
        ))
    );
    let (committed_agent, committed_id, after_seq) = observed.committed.unwrap();
    assert_eq!(&committed_agent, target_agent);
    assert_eq!(&committed_id, target_id);
    assert!(after_seq.is_some());
    assert_eq!(observed.order, ["requested", "committed", "ended"]);
    let target = fixture.session("delegate-agent", target_id).await?;
    let expected_prompt = if explicit {
        "finish explicit work"
    } else {
        "finish generated work"
    };
    let child_entries = wait_for_handoff_target(&fixture.log(&target), expected_prompt).await?;
    assert_handoff_target_log(&child_entries, expected_prompt);
    Ok(())
}

fn assert_provider_output(entries: &[(u64, SessionLogEntry)], explicit: bool) {
    let output = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results.first(),
            _ => None,
        })
        .expect("provider result must be persisted");
    assert_eq!(output.output["action"], "switch_agent");
    assert!(output.output.get("handoff").is_none());
    assert_eq!(
        output.output.get("session_id").and_then(|id| id.as_str()),
        explicit.then_some(EXPLICIT_TARGET_ID)
    );
}

async fn wait_for_registration(
    jetstream: &async_nats::jetstream::Context,
    key: &str,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !registration_exists(jetstream, key).await? {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

async fn registration_exists(
    jetstream: &async_nats::jetstream::Context,
    key: &str,
) -> Result<bool> {
    let Ok(registry) = jetstream
        .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
        .await
    else {
        return Ok(false);
    };
    Ok(registry.get(key).await?.is_some())
}
