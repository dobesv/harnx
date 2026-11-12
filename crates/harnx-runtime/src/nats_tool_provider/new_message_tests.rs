//! An opted-in call learns about a new message for its session: in its
//! request when one is already waiting at dispatch, by control message once
//! it is registered otherwise.
use super::*;
use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_toolset::ControlKind;
use serde_json::json;

const SESSION: &str = "new-message-session";

fn wait_route(stop_on_new_message: bool) -> RegisteredTool {
    RegisteredTool {
        server: "____time".into(),
        selector_server: "time".into(),
        raw_name: "wait".into(),
        stop_on_new_message,
        ..Default::default()
    }
}

async fn session_provider(
) -> anyhow::Result<(NatsToolProvider, crate::nats_test_common::NatsServerHandle)> {
    let broker = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(broker.url()).await?;
    let mut provider = super::tests::make_test_provider(&client).await;
    provider.parent_session_id = Some(SESSION.into());
    Ok((provider, broker))
}

fn prepare(provider: &NatsToolProvider, route: &RegisteredTool) -> PendingToolRequest {
    provider
        .prepare_request(json!({"seconds": 30}), route, None)
        .unwrap_or_else(|_| panic!("prepare request"))
}

fn starts_with_message(provider: &NatsToolProvider, route: &RegisteredTool) -> bool {
    prepare(provider, route)
        .durable
        .capabilities
        .contains(CAPABILITY_NEW_MESSAGE_PENDING)
}

#[test]
fn new_message_opt_in_is_read_from_the_tool_spec() {
    let spec = |name: &str| ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.into(),
        description: String::new(),
        input_schema: json!({"type": "object"}),
        idempotent_hint: false,
        read_only_hint: true,
        timeout_secs: None,
        meta: None,
    };
    let registration = Registration {
        package: None,
        config: String::new(),
        server: "time".into(),
        tools: vec![spec("wait").with_stop_on_new_message(), spec("now")],
        schema_version: 1,
        proto_version: harnx_toolset_server::TOOL_PROTOCOL_VERSION,
    };
    let (tools, _) = build_registered_tools(None, vec![registration]);
    let opted_in = |name: &str| tools[name].stop_on_new_message;
    assert_eq!([opted_in("time_wait"), opted_in("time_now")], [true, false]);
}

#[tokio::test]
async fn new_message_pending_at_dispatch_rides_in_an_opted_in_request() -> anyhow::Result<()> {
    let (provider, _broker) = session_provider().await?;
    assert!(!starts_with_message(&provider, &wait_route(true)));
    provider.in_flight.pending_message_targets(SESSION, 10);
    assert!(starts_with_message(&provider, &wait_route(true)));
    assert!(
        !starts_with_message(&provider, &wait_route(false)),
        "only a tool that opted in is told"
    );
    provider.in_flight.note_messages_consumed(SESSION, 10);
    assert!(
        !starts_with_message(&provider, &wait_route(true)),
        "the turn has taken the message in"
    );
    Ok(())
}

/// The watcher can note a message while a request is built and journaled,
/// before its call is registered: it then has no call to signal, and the
/// request doesn't carry the capability. Registration has to catch it.
#[tokio::test]
async fn new_message_noted_before_registration_signals_the_registered_call() -> anyhow::Result<()> {
    let (provider, _broker) = session_provider().await?;
    let client = provider.client.clone();
    let mut controls = client
        .subscribe(provider.instance_id.control_subject())
        .await?;
    let requests_subject = client.new_inbox();
    let _requests = client.subscribe(requests_subject.clone()).await?;
    client.flush().await?;
    let mut pending = prepare(&provider, &wait_route(true));
    pending.subject = requests_subject;
    let call_id = pending.call_id.clone();
    assert!(provider
        .in_flight
        .pending_message_targets(SESSION, 7)
        .is_empty());

    let abort = create_abort_signal();
    let wait = provider.await_response(pending, &abort);
    tokio::pin!(wait);
    let message = tokio::select! {
        _ = &mut wait => anyhow::bail!("the call has no reply to return"),
        message = tokio::time::timeout(Duration::from_secs(5), controls.next()) => {
            message?.context("control subscription closed")?
        }
    };
    let control: ControlMessage = serde_json::from_slice(&message.payload)?;
    assert_eq!(control.kind, ControlKind::NewMessage);
    assert_eq!(
        [control.call_id, control.session_id],
        [call_id, SESSION.into()]
    );
    Ok(())
}
