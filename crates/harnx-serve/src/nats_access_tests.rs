//! Unit tests for the typed broker-unavailability classification
//! ([`is_nats_unavailable`](crate::nats_access::is_nats_unavailable)) and the
//! URL-credential redaction of logged error chains.
//!
//! Per `AGENTS.md:434`, classification tests must not match error text and
//! must be built from real async-nats error values: the broker-backed tests
//! below produce no-responders and retry-deadline errors from real
//! async-nats calls against a live broker in a degraded condition, and the
//! wrapper-kind tests deserialize the real JetStream API error payloads with
//! the offline codes and wrap them in async-nats's own public wrapper
//! constructors.

use std::time::Duration;

use anyhow::{anyhow, Context as _};

use crate::nats_redaction::redact_url_credentials;
use harnx_nats_common::recovery;

use crate::nats_access::{is_nats_unavailable, sanitize_nats_cluster_error, sanitized_error_chain};

#[test]
fn connect_errors_and_the_sanitize_marker_are_unavailable() {
    // A real connect failure from async-nats, wrapped the way
    // `Config::nats_client` wraps it: the context message stays, the typed
    // cause stays in the chain.
    let connect = async_nats::ConnectError::with_source(
        async_nats::ConnectErrorKind::Io,
        std::io::Error::other("connection refused"),
    );
    let error = anyhow::Error::new(connect)
        .context("Failed to connect to NATS cluster 'down' at 'nats://127.0.0.1:1'");
    assert!(is_nats_unavailable(&error));

    // The sanitize path replaces the chain with the marker; the message and
    // the classification both keep their shape.
    let sanitized = sanitize_nats_cluster_error("down", error);
    assert!(is_nats_unavailable(&sanitized));
    assert_eq!(sanitized.to_string(), "NATS cluster 'down' is unavailable");
    assert!(!sanitized.to_string().contains("nats://"));
}

/// The JetStream API wrappers the upload paths produce, against a live
/// broker whose JetStream domain maps nowhere: every `$JS.<domain>.API`
/// request has no responder, which is the deterministic degraded condition
/// the no-responders branches of the classifier exist for (TCP up, JetStream
/// API unreachable). Each call produces the real wrapper chain its upload
/// path would produce, and each must classify as unavailability.
#[tokio::test]
async fn real_no_responder_wrapper_chains_are_unavailable() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::with_domain(client.clone(), "nowhere");

    // The bucket-ensure path: `ensure_attachments_bucket` starts with
    // `create_object_store`, whose `BucketCreate` source is the real
    // `CreateStreamError` async-nats built from the no-responders request
    // (`CreateStreamErrorKind::JetStreamUnavailable`).
    let error = jetstream
        .create_object_store(async_nats::jetstream::object_store::Config {
            bucket: "harnx_attachments".to_string(),
            ..Default::default()
        })
        .await
        .err()
        .context("mapped-domain create_object_store must fail")?;
    let error = anyhow::Error::new(error).context("ensure the attachments bucket");
    assert!(
        is_nats_unavailable(&error),
        "create_object_store: {error:#}"
    );

    // The open path: `get_object_store` wraps `GetStreamError`, whose
    // `Request` source is the real JetStream-context `RequestError`
    // (`NoResponders`).
    let error = jetstream
        .get_object_store("harnx_attachments")
        .await
        .err()
        .context("mapped-domain get_object_store must fail")?;
    let error = anyhow::Error::new(error).context("open the attachments bucket");
    assert!(is_nats_unavailable(&error), "get_object_store: {error:#}");

    // The session-capture path: `get_key_value` wraps the same
    // `GetStreamError` chain.
    let error = jetstream
        .get_key_value("SESSION_METADATA")
        .await
        .err()
        .context("mapped-domain get_key_value must fail")?;
    let error = anyhow::Error::new(error).context("open the session metadata bucket");
    assert!(is_nats_unavailable(&error), "get_key_value: {error:#}");

    // The bare stream lookup the memo-refresh probe uses.
    let error = jetstream
        .get_stream("OBJ_harnx_attachments")
        .await
        .err()
        .context("mapped-domain get_stream must fail")?;
    let error = anyhow::Error::new(error).context("probe the attachment stream");
    assert!(is_nats_unavailable(&error), "get_stream: {error:#}");

    // A core round trip with no responder at all.
    let error = client
        .request(
            "nowhere.no.responders.service",
            bytes::Bytes::from_static(b"payload"),
        )
        .await
        .err()
        .context("core request to an unmapped subject must fail")?;
    let error = anyhow::Error::new(error).context("round trip the JetStream API");
    assert!(is_nats_unavailable(&error), "core request: {error:#}");
    Ok(())
}

/// A leader read that keeps failing on transport exhausts
/// `recovery::retry_until`'s deadline; the typed `RecoveryDeadlineExceeded`
/// marker it returns must classify as unavailability, replacing the old
/// context-string match.
#[tokio::test]
async fn recovery_deadline_is_unavailable() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::with_domain(client, "nowhere");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    // Each attempt outlives the retry window, the way a hung request does
    // when a broker stops answering: the deadline fires mid-attempt and
    // `retry_until` returns the typed marker, not the raw error.
    let result = recovery::retry_until(
        deadline,
        || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            jetstream.get_stream("OBJ_harnx_attachments").await
        },
        |_| true,
    )
    .await;
    let error = result
        .err()
        .context("retry_until against no responders must fail")?;
    let chain = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>();
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<recovery::RecoveryDeadlineExceeded>()
            .is_some()),
        "the retry window must end in the typed deadline marker: {chain:?}"
    );
    assert!(is_nats_unavailable(&error), "deadline chain: {chain:?}");
    Ok(())
}

/// Wrapper kinds built from real JetStream API error payloads with the
/// offline error codes, wrapped the way async-nats wraps a server API
/// error (`jetstream::Error` moved into the wrapper's kind): these are the
/// only paths that surface `CLUSTER_NOT_AVAILABLE` and `STREAM_OFFLINE`.
#[test]
fn jetstream_offline_error_codes_in_wrappers_are_unavailable() {
    fn api_error(code: async_nats::jetstream::ErrorCode) -> async_nats::jetstream::Error {
        // `jetstream::Error` has no public constructor; deserialize the
        // real server API error shape with the code under test.
        let json = format!(
            r#"{{"code":503,"err_code":{},"description":"temporarily unavailable"}}"#,
            code.0
        );
        serde_json::from_str(&json).expect("parse jetstream error")
    }
    use async_nats::jetstream::{
        context::{CreateStreamError, CreateStreamErrorKind, GetStreamError, GetStreamErrorKind},
        stream::{LastRawMessageError, LastRawMessageErrorKind},
    };
    for code in [
        async_nats::jetstream::ErrorCode::CLUSTER_NOT_AVAILABLE,
        async_nats::jetstream::ErrorCode::STREAM_OFFLINE,
    ] {
        let api_error = api_error(code);
        assert_eq!(api_error.error_code(), code);

        // Stream lookup failure with an offline code.
        let get_stream = anyhow::Error::new(GetStreamError::new(GetStreamErrorKind::JetStream(
            api_error.clone(),
        )))
        .context("open the session metadata bucket");
        assert!(is_nats_unavailable(&get_stream), "{code:?} get_stream");

        // Stream create failure with an offline code.
        let create_stream = anyhow::Error::new(CreateStreamError::new(
            CreateStreamErrorKind::JetStream(api_error.clone()),
        ))
        .context("ensure the attachments bucket");
        assert!(
            is_nats_unavailable(&create_stream),
            "{code:?} create_stream"
        );

        // Leader raw-message read failure with an offline code.
        let raw_message = anyhow::Error::new(LastRawMessageError::new(
            LastRawMessageErrorKind::JetStream(api_error),
        ))
        .context("read the media leader metadata");
        assert!(is_nats_unavailable(&raw_message), "{code:?} raw_message");
    }

    // A bare `jetstream::Error` can still surface on paths that hand the
    // server's API error through unchanged.
    let bare = anyhow::Error::new(api_error(
        async_nats::jetstream::ErrorCode::CLUSTER_NOT_AVAILABLE,
    ));
    assert!(is_nats_unavailable(&bare));

    // The bare no-responders and timeout kinds async-nats maps its request
    // errors to, with no source retained: the wrapper kind alone carries
    // the failure mode.
    let no_responders = anyhow::Error::new(CreateStreamError::new(
        CreateStreamErrorKind::JetStreamUnavailable,
    ))
    .context("ensure the attachments bucket");
    assert!(is_nats_unavailable(&no_responders));
    let timed_out = anyhow::Error::new(CreateStreamError::new(CreateStreamErrorKind::TimedOut))
        .context("ensure the attachments bucket");
    assert!(is_nats_unavailable(&timed_out));
}

/// The typed KV write and direct-get kinds the first-prompt reservation
/// surfaces after connect (review D1): a `kv::Store::create` whose broker
/// stalled or died mid-write — the ack timeout, whose `From<UpdateError>`
/// conversion re-labels the kind as `CreateErrorKind::Publish` while
/// keeping the `UpdateError` source, and the bare update one level down —
/// the publish round trip's timed-out ack, broken pipe and lost stream,
/// and a direct get with no responders whose conversion drops its source
/// so the status is the only carrier. Built from async-nats's own public
/// wrapper constructors; no error text is matched.
#[test]
fn kv_write_and_direct_get_outage_kinds_are_unavailable() {
    use async_nats::jetstream::kv::{CreateError, CreateErrorKind, UpdateError, UpdateErrorKind};

    // `kv::Store::create` on a broker stalled after connect: the put's ack
    // wait times out. The From conversion both keeps the UpdateError in
    // the chain and re-labels its kind — the classifier must catch either.
    let stalled = CreateError::with_source(
        CreateErrorKind::Publish,
        UpdateError::new(UpdateErrorKind::TimedOut),
    );
    let error = anyhow::Error::new(stalled).context("reserve session metadata");
    assert!(is_nats_unavailable(&error), "kv create ack timeout");

    let bare_update = anyhow::Error::new(UpdateError::new(UpdateErrorKind::TimedOut));
    assert!(is_nats_unavailable(&bare_update), "kv update ack timeout");

    for kind in [
        async_nats::jetstream::context::PublishErrorKind::TimedOut,
        async_nats::jetstream::context::PublishErrorKind::BrokenPipe,
        async_nats::jetstream::context::PublishErrorKind::StreamNotFound,
    ] {
        let publish = anyhow::Error::new(async_nats::jetstream::context::PublishError::new(kind))
            .context("publish the reservation put");
        assert!(is_nats_unavailable(&publish), "{kind:?} publish");
    }

    let no_responders = anyhow::Error::new(async_nats::jetstream::stream::DirectGetError::new(
        async_nats::jetstream::stream::DirectGetErrorKind::ErrorResponse(
            async_nats::StatusCode::NO_RESPONDERS,
            "no responders".to_string(),
        ),
    ));
    assert!(
        is_nats_unavailable(&no_responders),
        "direct get no responders"
    );
}

/// The KV and direct-get kinds that are broker answers, not outages
/// (review D1): an existing key, an invalid key, a racing revision, a
/// client-side payload rejection and an answered-but-negative direct get
/// must stay non-503 — no blanket unavailability.
#[test]
fn kv_store_answers_and_direct_get_responses_stay_not_unavailable() {
    use async_nats::jetstream::kv::{CreateErrorKind, UpdateErrorKind};
    use async_nats::jetstream::stream::DirectGetErrorKind;

    let already_exists = anyhow::Error::new(async_nats::jetstream::kv::CreateError::new(
        CreateErrorKind::AlreadyExists,
    ))
    .context("reserve session metadata");
    assert!(!is_nats_unavailable(&already_exists));

    let wrong_revision = anyhow::Error::new(async_nats::jetstream::kv::UpdateError::new(
        UpdateErrorKind::WrongLastRevision,
    ))
    .context("reserve session metadata");
    assert!(!is_nats_unavailable(&wrong_revision));

    let max_payload = anyhow::Error::new(async_nats::jetstream::context::PublishError::new(
        async_nats::jetstream::context::PublishErrorKind::MaxPayloadExceeded,
    ))
    .context("publish the reservation put");
    assert!(!is_nats_unavailable(&max_payload));

    // A direct get the broker answered with an ordinary status: not found,
    // not an outage.
    let not_found = anyhow::Error::new(async_nats::jetstream::stream::DirectGetError::new(
        DirectGetErrorKind::ErrorResponse(
            async_nats::StatusCode::NOT_FOUND,
            "message not found".to_string(),
        ),
    ));
    assert!(!is_nats_unavailable(&not_found));
}

#[test]
fn store_failures_are_not_unavailable() {
    // A client-side payload rejection, request errors without an
    // unavailability kind, JetStream errors with store-level codes and plain
    // store failures must stay 500-class failures.
    let max_payload = anyhow::Error::new(async_nats::RequestError::new(
        async_nats::RequestErrorKind::MaxPayloadExceeded,
    ));
    assert!(!is_nats_unavailable(&max_payload));

    let other_request = anyhow::Error::new(async_nats::RequestError::new(
        async_nats::RequestErrorKind::Other,
    ))
    .context("failed to get Object Store");
    assert!(!is_nats_unavailable(&other_request));

    // Error code 10109: "invalid operation on sealed stream", the fixture
    // of the 500-body secrecy test — at the wrapper level too.
    let sealed: async_nats::jetstream::Error = serde_json::from_str(
        r#"{"code":400,"err_code":10109,"description":"invalid operation on sealed stream"}"#,
    )
    .expect("parse jetstream error");
    let sealed = anyhow::Error::new(sealed).context("failed publishing object chunks");
    assert!(!is_nats_unavailable(&sealed));
    let sealed_wrapper = anyhow::Error::new(async_nats::jetstream::stream::LastRawMessageError::new(
        async_nats::jetstream::stream::LastRawMessageErrorKind::JetStream(
            serde_json::from_str::<async_nats::jetstream::Error>(
                r#"{"code":400,"err_code":10109,"description":"invalid operation on sealed stream"}"#,
            )
            .expect("parse jetstream error"),
        ),
    ));
    assert!(!is_nats_unavailable(&sealed_wrapper));

    // A stream-not-found lookup is an absence answer, not unavailability.
    let stream_not_found_json = format!(
        r#"{{"code":400,"err_code":{},"description":"stream not found"}}"#,
        async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND.0
    );
    let stream_not_found: async_nats::jetstream::Error =
        serde_json::from_str(&stream_not_found_json).expect("parse jetstream error");
    let lookup = anyhow::Error::new(async_nats::jetstream::context::GetStreamError::new(
        async_nats::jetstream::context::GetStreamErrorKind::JetStream(stream_not_found),
    ));
    assert!(!is_nats_unavailable(&lookup));

    assert!(!is_nats_unavailable(&anyhow!(
        "failed to put media blob 'media/key'"
    )));
}

#[test]
fn url_credentials_are_redacted_from_error_chains() {
    // The context `Config::nats_client` adds echoes the cluster URL; with
    // userinfo, the credentials must not survive the log rendering.
    let connect = async_nats::ConnectError::with_source(
        async_nats::ConnectErrorKind::Io,
        std::io::Error::other("connection refused"),
    );
    let error = anyhow::Error::new(connect)
        .context("Failed to connect to NATS cluster 'secure' at 'nats://alice:secret@127.0.0.1:1'");
    let rendered = sanitized_error_chain(&error);
    assert!(
        !rendered.contains("alice:secret"),
        "credentials must be redacted: {rendered}"
    );
    assert!(
        rendered.contains("nats://***@127.0.0.1:1"),
        "the URL stays identifiable without its userinfo: {rendered}"
    );
}

#[test]
fn url_credential_redaction_covers_realistic_shapes() {
    // Comma-separated multi-URL server lists: every URL's userinfo goes.
    let list = "nats://u1:p1@h1:4222,nats://u2:p2@h2:4222,ws://h3:4222";
    assert_eq!(
        redact_url_credentials(list),
        "nats://***@h1:4222,nats://***@h2:4222,ws://h3:4222",
        "every comma-separated URL with credentials is redacted"
    );

    // Schemeless endpoint with credentials, as a context message that
    // dropped the scheme renders it.
    assert_eq!(
        redact_url_credentials("connect to user:pass@broker:4222 failed"),
        "connect to ***@broker:4222 failed",
        "schemeless credentials are redacted"
    );

    // Query strings can carry NATS auth tokens: redacted whole, presence kept.
    assert_eq!(
        redact_url_credentials("nats://broker:4222?token=s3cret&user=bob"),
        "nats://broker:4222?***",
        "the query string is redacted"
    );
    assert_eq!(
        redact_url_credentials("ws://u:p@broker:4222?token=s3cret"),
        "ws://***@broker:4222?***",
        "userinfo and query string are redacted together"
    );

    // Non-credential text passes through: plain URLs, email addresses.
    let untouched = "contact alice@harnx.io about nats://plain:4222 today";
    assert_eq!(
        redact_url_credentials(untouched),
        untouched,
        "URLs without credentials and plain email addresses stay unchanged"
    );

    // Schemeless endpoints carry their secrets in the query string too:
    // mask it with or without userinfo, with or without a path, and leave
    // plain email addresses and prose with a question mark untouched.
    assert_eq!(
        redact_url_credentials("broker.example:4222/path?token=sample"),
        "broker.example:4222/path?***",
        "a schemeless host:port/path query is redacted"
    );
    assert_eq!(
        redact_url_credentials("broker.example:4222?token=sample"),
        "broker.example:4222?***",
        "a schemeless host:port query is redacted"
    );
    assert_eq!(
        redact_url_credentials("user:pass@host:4222?token=sample"),
        "***@host:4222?***",
        "schemeless userinfo and query are redacted together"
    );
    assert_eq!(
        redact_url_credentials("contact alice@harnx.io or reply? no"),
        "contact alice@harnx.io or reply? no",
        "prose with an email and a question mark stays unchanged"
    );
    assert_eq!(
        redact_url_credentials("see 10.30 today? maybe"),
        "see 10.30 today? maybe",
        "a colon tail that is not digits-then-path stays unchanged"
    );
    assert_eq!(
        redact_url_credentials("see 10:30am? maybe"),
        "see 10:30am? maybe",
        "digits followed by something other than a path stays unchanged"
    );

    // R3-1: a comma inside credentials or a query string must not split the
    // URL — the tail after the comma still gets masked with the whole URL.
    assert_eq!(
        redact_url_credentials("nats://u:sample,tail@host:4222"),
        "nats://***@host:4222",
        "a comma in userinfo does not return the URL unmasked"
    );
    assert_eq!(
        redact_url_credentials("wss://host/path?auth_token=sample,tail"),
        "wss://host/path?***",
        "a comma in a query string does not leak the tail"
    );

    // The round-1 shapes keep their rendering.
    assert_eq!(
        redact_url_credentials(
            "connect to nats://user:pass@broker:4222 and tls://k:v@other:4223, then nats://plain:4224 ok"
        ),
        "connect to nats://***@broker:4222 and tls://***@other:4223, then nats://plain:4224 ok"
    );
}
