//! CAS and fencing behavior of the registration handle (T2, decision 5):
//! stale writers must never overwrite or delete a newer connection's
//! registration.

use crate::common;
use anyhow::Result;
use harnx_core::instance::ServerScope;
use harnx_toolset::{Registration, ToolSpec, TOOL_PROTOCOL_VERSION};
use harnx_toolset_server::{
    ConditionalDelete, RegistrationHandle, RegistrationHandleError, RegistrationIdentity,
    TOOL_SCHEMA_VERSION,
};
use serde_json::json;

/// The per-principal audience selector; re-exported from the crate root as
/// `HandleAudience` to avoid confusion with the wire-format audience enum.
type RegistrationAudience = harnx_toolset_server::HandleAudience;

fn registration_payload(user_id: Option<&str>, ownerless_only: bool) -> Registration {
    Registration {
        package: None,
        config: String::new(),
        server: "creds".to_string(),
        tools: vec![ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "personal".to_string(),
            description: "a user-scoped tool".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: Some(30),
            meta: None,
        }],
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
        user_id: user_id.map(str::to_string),
        ownerless_only,
        connection_id: Some("conn-1".to_string()),
    }
}

/// A spawned broker plus a handle over its registry. The handle alone would
/// let the `NatsServerHandle` drop at the end of this function, which kills
/// the child process, so the fixture keeps both alive for the test.
struct Fixture {
    _server: common::NatsServerHandle,
    handle: RegistrationHandle,
}

async fn handle_with_server() -> Result<Option<Fixture>> {
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let handle = RegistrationHandle::open(
        &client,
        1,
        ServerScope::new(),
        RegistrationIdentity::new(None, ""),
        "creds",
    )
    .await?;
    Ok(Some(Fixture {
        _server: server,
        handle,
    }))
}

/// The Alice-audience tests' shared opening: a live fixture and Alice's
/// user audience.
async fn handle_and_alice_audience() -> Result<Option<(Fixture, RegistrationAudience)>> {
    let Some(fx) = handle_with_server().await? else {
        return Ok(None);
    };
    Ok(Some((fx, RegistrationAudience::User("alice".to_string()))))
}

#[tokio::test]
async fn publish_is_create_once_and_conflicts_with_a_live_value() -> Result<()> {
    harnx_core::require_nextest();
    let Some((fx, audience)) = handle_and_alice_audience().await? else {
        return Ok(());
    };
    let key = fx.handle.key(&audience);

    let revision = fx
        .handle
        .publish(&audience, registration_payload(Some("alice"), false))
        .await?;
    assert!(revision > 0);
    let stored = fx
        .handle
        .read(&audience)
        .await?
        .expect("registration is live");
    assert_eq!(stored.revision, revision);
    assert_eq!(stored.registration.connection_id.as_deref(), Some("conn-1"));
    // The key is the user-audience form, not the bare global one.
    let segment = harnx_toolset::user_key_segment("alice");
    assert!(
        key.ends_with(&format!(".u.{segment}")),
        "unexpected key {key}"
    );

    // A second publisher for the same user gets a conflict, never a blind overwrite.
    match fx
        .handle
        .publish(&audience, registration_payload(Some("alice"), false))
        .await
    {
        Err(RegistrationHandleError::Conflict) => {}
        other => panic!("expected conflict, got {other:?}"),
    }
    Ok(())
}

/// Publish Alice's registration, then move it to `conn-2` so a caller still
/// holding the first revision races a newer write.
async fn publish_then_move_to_conn_2(
    fx: &Fixture,
    audience: &RegistrationAudience,
) -> Result<(u64, u64)> {
    let revision = fx
        .handle
        .publish(audience, registration_payload(Some("alice"), false))
        .await?;
    let mut moved = registration_payload(Some("alice"), false);
    moved.connection_id = Some("conn-2".to_string());
    let latest = fx.handle.update(audience, moved, revision).await?;
    Ok((revision, latest))
}

#[tokio::test]
async fn update_conflicts_on_a_stale_revision() -> Result<()> {
    harnx_core::require_nextest();
    let Some((fx, audience)) = handle_and_alice_audience().await? else {
        return Ok(());
    };

    let (revision, latest) = publish_then_move_to_conn_2(&fx, &audience).await?;

    // The first connection's refresh tick still holds the old revision: it
    // must fail, not clobber the newer connection's write.
    match fx
        .handle
        .update(
            &audience,
            registration_payload(Some("alice"), false),
            revision,
        )
        .await
    {
        Err(RegistrationHandleError::Conflict) => {}
        other => panic!("expected conflict, got {other:?}"),
    }
    let stored = fx
        .handle
        .read(&audience)
        .await?
        .expect("registration is live");
    assert_eq!(stored.revision, latest);
    assert_eq!(stored.registration.connection_id.as_deref(), Some("conn-2"));
    Ok(())
}

#[tokio::test]
async fn delete_is_fenced_by_connection_id_and_revision() -> Result<()> {
    harnx_core::require_nextest();
    let Some((fx, audience)) = handle_and_alice_audience().await? else {
        return Ok(());
    };
    fx.handle
        .publish(&audience, registration_payload(Some("alice"), false))
        .await?;
    let stored = fx
        .handle
        .read(&audience)
        .await?
        .expect("registration is live");

    // A stale writer that observed an older connection's registration: the
    // stored connection_id differs, so the delete is refused and the newer
    // connection's registration survives.
    let outcome = fx
        .handle
        .delete(&audience, stored.revision, "conn-0")
        .await?;
    assert_eq!(outcome, ConditionalDelete::ConnectionMismatch);
    assert!(fx.handle.read(&audience).await?.is_some());

    // Matching connection and revision deletes.
    let outcome = fx
        .handle
        .delete(&audience, stored.revision, "conn-1")
        .await?;
    assert_eq!(outcome, ConditionalDelete::Deleted);
    assert!(fx.handle.read(&audience).await?.is_none());

    // Deleting something already gone is not an error: the caller's goal is
    // achieved.
    let outcome = fx
        .handle
        .delete(&audience, stored.revision, "conn-1")
        .await?;
    assert_eq!(outcome, ConditionalDelete::Deleted);
    Ok(())
}

#[tokio::test]
async fn delete_conflicts_when_the_revision_moved() -> Result<()> {
    harnx_core::require_nextest();
    let Some((fx, audience)) = handle_and_alice_audience().await? else {
        return Ok(());
    };
    let (revision, _latest) = publish_then_move_to_conn_2(&fx, &audience).await?;

    // The stale writer read revision N but the stored revision moved: the
    // revision CAS keeps its connection check from racing the newer write.
    match fx.handle.delete(&audience, revision, "conn-1").await {
        Err(RegistrationHandleError::Conflict) => {}
        other => panic!("expected conflict, got {other:?}"),
    }
    Ok(())
}

/// An ownerless-only registration publishes under the `.anon` key and follows
/// the same CAS lifecycle.
#[tokio::test]
async fn ownerless_only_audience_follows_the_same_lifecycle() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = handle_with_server().await? else {
        return Ok(());
    };
    let audience = RegistrationAudience::OwnerlessOnly;
    assert!(fx.handle.key(&audience).ends_with(".anon"));

    let revision = fx
        .handle
        .publish(&audience, registration_payload(None, true))
        .await?;
    let stored = fx
        .handle
        .read(&audience)
        .await?
        .expect("registration is live");
    assert_eq!(stored.revision, revision);
    assert!(stored.registration.ownerless_only);
    assert_eq!(stored.registration.user_id, None);

    let outcome = fx.handle.delete(&audience, revision, "conn-1").await?;
    assert_eq!(outcome, ConditionalDelete::Deleted);
    Ok(())
}

/// The handle refuses payloads that don't match their audience, so a
/// misconfigured publisher can't silently publish to the wrong key.
#[tokio::test]
async fn payload_must_match_its_audience() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = handle_with_server().await? else {
        return Ok(());
    };
    // Ownerless-only payload under a user key.
    match fx
        .handle
        .publish(
            &RegistrationAudience::User("alice".to_string()),
            registration_payload(None, true),
        )
        .await
    {
        Err(RegistrationHandleError::Invalid(_)) => {}
        other => panic!("expected invalid, got {other:?}"),
    }
    // User payload under the ownerless-only key.
    match fx
        .handle
        .publish(
            &RegistrationAudience::OwnerlessOnly,
            registration_payload(Some("alice"), false),
        )
        .await
    {
        Err(RegistrationHandleError::Invalid(_)) => {}
        other => panic!("expected invalid, got {other:?}"),
    }
    // The wrong user id in the payload is invalid too.
    match fx
        .handle
        .publish(
            &RegistrationAudience::User("alice".to_string()),
            registration_payload(Some("bob"), false),
        )
        .await
    {
        Err(RegistrationHandleError::Invalid(_)) => {}
        other => panic!("expected invalid, got {other:?}"),
    }
    Ok(())
}
