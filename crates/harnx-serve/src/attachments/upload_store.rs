//! The upload write path: one upload request's handle on the attachments
//! object store, its session-incarnation capture and recheck, and the
//! store-failure classification of the upload error contract (F3/F6).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use harnx_blob_store::media::AttachmentsBucket;
use harnx_core::cid_url::{CidUrl, SessionRef};
use http::StatusCode;
use serde_json::json;

use super::AttachmentUploadError;
use crate::{
    nats_access::{is_nats_unavailable, nats_cluster_unavailable, sanitized_error_chain},
    session_actor::ResolvedAgentTarget,
    Server,
};

/// One upload request's lazy handle on the shared attachments object store.
///
/// JetStream is only taken when the first valid part is stored, so a request
/// rejected by validation before any part is accepted (unsupported MIME
/// type, oversized first part, bad multipart framing) never needs a broker
/// and keeps its 4xx even when the broker is down. Once a part is accepted
/// the broker has been used; a later part's validation error still wins, but
/// a broker failure storing it can surface first.
pub(crate) struct AttachmentObjectStore<'a> {
    server: &'a Server,
    target: &'a ResolvedAgentTarget,
    /// The session the upload's refs will address; its incarnation is
    /// rechecked after each acknowledged put.
    session: &'a SessionRef,
    /// The session incarnation the guard-equivalent read proved at request
    /// start, with access rules on (M2). `None` means rules are off and the
    /// capture reads with D7 semantics: absence is "never created", not
    /// "deleted mid-upload".
    proven_incarnation: Option<DateTime<Utc>>,
    bucket: Option<AttachmentsBucket>,
    /// The broker connection the bucket rides on, taken with it. A put
    /// whose chunk or metadata publish failed carries only stringified
    /// causes (`ObjectStore::put` has no typed JetStream kind to
    /// downcast), so classification falls back to the client's typed
    /// connection state — plus any typed evidence the blob store chained
    /// into the failure, such as a failed dedup probe's leader-read error.
    client: Option<async_nats::Client>,
    /// The session incarnation captured at the first valid part; `None`
    /// until then.
    reservation: Option<SessionReservation>,
}

/// The session incarnation an upload addresses, leader-read once at the
/// request's first valid part.
enum SessionReservation {
    /// The session's metadata exists; `created_at` is the incarnation every
    /// acknowledged put must recheck before its ref is handed out. The
    /// metadata store is kept for the rechecks (boxed: it is far larger
    /// than the variant needs, and only one is held per request).
    Reserved {
        created_at: DateTime<Utc>,
        metadata: Box<harnx_runtime::nats_session_metadata::SessionMetadataStore>,
    },
    /// The session has no metadata record — a never-created owner (D7), or
    /// an owner whose record was deleted before this upload's first part.
    /// D7 allows the upload; there is no incarnation to protect and no
    /// recheck to make.
    Unreserved,
    /// The upload carried a proof (access rules on, M2) and the capture no
    /// longer matched it: the session was deleted or replaced between the
    /// proof at request start and this first part. Nothing has been
    /// written yet; `put` fails the upload with `SessionGone` before the
    /// store write.
    Gone,
}

/// Why one accepted part produced no ref.
#[derive(Debug)]
pub(crate) enum UploadPutError {
    /// The object store failed; classified to 503/500 per F3
    /// (`attachment_store_error`).
    Store(anyhow::Error),
    /// The session was confirmed gone or replaced after this part's put was
    /// acknowledged. No ref may be returned; the object version this put
    /// created has been cleaned up.
    SessionGone,
}

impl<'a> AttachmentObjectStore<'a> {
    pub(crate) fn new(
        server: &'a Server,
        target: &'a ResolvedAgentTarget,
        session: &'a SessionRef,
        proven_incarnation: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            server,
            target,
            session,
            proven_incarnation,
            bucket: None,
            client: None,
            reservation: None,
        }
    }

    /// Write one accepted blob to the object store. The first call takes
    /// JetStream, ensures the bucket and captures the session incarnation;
    /// later parts reuse all three.
    // qual:allow(iosp) reason: "the lazy take-then-capture-then-put-then-recheck order is the upload's safety contract (F6); extracting a step would let a part be stored without its incarnation captured or rechecked in between."
    pub(crate) async fn put(
        &mut self,
        url: &CidUrl,
        bytes: &[u8],
        mime: &str,
    ) -> Result<(), UploadPutError> {
        if self.bucket.is_none() {
            let (client, bucket) = self
                .server
                .attachments_bucket(self.target)
                .await
                .map_err(UploadPutError::Store)?;
            self.client = Some(client);
            self.bucket = Some(bucket);
        }
        let bucket = self
            .bucket
            .as_ref()
            .context("attachment bucket was ensured for this upload")
            .map_err(UploadPutError::Store)?;
        let client = self
            .client
            .as_ref()
            .context("broker connection was taken with the attachment bucket")
            .map_err(UploadPutError::Store)?;
        // Capture the incarnation from the leader at the first valid part,
        // before this upload's first put. Never provisions metadata: without
        // a proof (rules off), a missing bucket or record means the session
        // was never created through the guard (D7) and the upload proceeds
        // unrechecked. With a proof (rules on), a capture that no longer
        // matches it fails the upload before the put (M2). Capture failures
        // classify like recheck failures (F3), so a disconnected broker at
        // capture time is 503, not 500.
        if self.reservation.is_none() {
            match self.capture_session_reservation(client).await {
                Ok(SessionReservation::Gone) => return Err(UploadPutError::SessionGone),
                Ok(reservation) => self.reservation = Some(reservation),
                Err(error) => return Err(self.classify_read_failure(client, error)),
            }
        }
        let outcome = self.store_part(bucket, client, url, bytes, mime).await?;
        self.recheck_session_after_put(bucket, client, url, &outcome)
            .await
    }

    /// Store one part, classifying a put failure exactly as F3 does.
    async fn store_part(
        &self,
        bucket: &AttachmentsBucket,
        client: &async_nats::Client,
        url: &CidUrl,
        bytes: &[u8],
        mime: &str,
    ) -> Result<harnx_blob_store::media::MediaPut, UploadPutError> {
        let blob = harnx_blob_store::media::MediaBlob {
            url,
            bytes,
            mime_type: mime,
        };
        match harnx_blob_store::media::put_media_versioned(bucket, blob).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(self.classify_put_failure(client, error).await),
        }
    }

    /// A put whose chunk or metadata publish failed carries only
    /// stringified JetStream causes, so the chain has no typed kinds to
    /// downcast. Two typed signals remain: the client's connection state at
    /// failure time, and any typed availability evidence the blob store
    /// chained into the failure — a dedup probe that also failed keeps its
    /// leader-read error in the chain (`put_failure` in
    /// `harnx-blob-store`), which names the same JetStream the put then
    /// failed against. Either signal that says unavailability makes this a
    /// 503; on a live connection without typed evidence it is a store
    /// failure, and a definitively missing stream resets the cluster's
    /// cached bucket so the next upload re-ensures it. The original chain
    /// is logged by `nats_cluster_unavailable`, not returned to the client.
    async fn classify_put_failure(
        &self,
        client: &async_nats::Client,
        error: anyhow::Error,
    ) -> UploadPutError {
        if client.connection_state() != async_nats::connection::State::Connected
            || is_nats_unavailable(&error)
        {
            return UploadPutError::Store(nats_cluster_unavailable(self.target.cluster(), error));
        }
        self.server
            .refresh_upload_bucket_on_missing_stream(self.target.cluster())
            .await;
        UploadPutError::Store(error)
    }

    /// After an acknowledged put, before its ref is handed out: recheck the
    /// session incarnation. Only a session that had a reserved incarnation
    /// is rechecked; a never-created owner (D7) has nothing to protect.
    async fn recheck_session_after_put(
        &self,
        bucket: &AttachmentsBucket,
        client: &async_nats::Client,
        url: &CidUrl,
        outcome: &harnx_blob_store::media::MediaPut,
    ) -> Result<(), UploadPutError> {
        let SessionReservation::Reserved {
            created_at,
            metadata,
        } = self
            .reservation
            .as_ref()
            .context("session reservation was captured for this upload")
            .map_err(UploadPutError::Store)?
        else {
            return Ok(());
        };
        // Test seam: park between the acknowledged put and the recheck.
        self.server.wait_at_upload_recheck_gate().await;
        match metadata
            .get_for_agent(&self.session.session_id, self.target.agent())
            .await
        {
            Ok(Some(record)) if record.metadata.created_at == *created_at => Ok(()),
            Ok(_) => {
                purge_version_of_gone_session(bucket, url, outcome).await;
                Err(UploadPutError::SessionGone)
            }
            Err(error) => Err(self.classify_read_failure(client, error)),
        }
    }

    /// The leader reads the upload makes (capture, recheck) may fail for
    /// transport or decode reasons; those are not evidence the session is
    /// gone. Classify like a put failure (unavailable broker → 503,
    /// anything else → 500) and never purge.
    fn classify_read_failure(
        &self,
        client: &async_nats::Client,
        error: anyhow::Error,
    ) -> UploadPutError {
        if client.connection_state() != async_nats::connection::State::Connected {
            return UploadPutError::Store(nats_cluster_unavailable(self.target.cluster(), error));
        }
        UploadPutError::Store(error)
    }

    /// Read the session's current incarnation from the metadata bucket,
    /// without ever provisioning metadata. With access rules on, the
    /// upload carries the incarnation proven at request start
    /// (`proven_incarnation`); a capture that no longer matches it means
    /// the session was deleted or replaced between the proof and this
    /// first part, so the upload answers `SessionGone` before writing
    /// anything (M2). Without a proof (rules off), absence stays "never
    /// created" (D7) and the upload proceeds unrechecked.
    async fn capture_session_reservation(
        &self,
        client: &async_nats::Client,
    ) -> Result<SessionReservation> {
        let jetstream = async_nats::jetstream::new(client.clone());
        let kv_store = match jetstream
            .get_key_value(harnx_runtime::nats_session_metadata::SESSION_METADATA_BUCKET)
            .await
        {
            Ok(kv_store) => kv_store,
            Err(error) if harnx_runtime::nats_admin::kv_bucket_missing(&error) => {
                return Ok(self.proven_capture(None));
            }
            Err(error) => {
                return Err(anyhow!(error).context("open the session metadata bucket"));
            }
        };
        let metadata = harnx_runtime::nats_session_metadata::SessionMetadataStore::from_store(
            kv_store,
            client.clone(),
        );
        match metadata
            .get_for_agent(&self.session.session_id, self.target.agent())
            .await
        {
            Ok(Some(record)) => {
                Ok(self.proven_capture(Some((record.metadata.created_at, metadata))))
            }
            Ok(None) => Ok(self.proven_capture(None)),
            Err(error) => Err(error.context("read the session's metadata")),
        }
    }

    /// Turn the capture read's outcome into a reservation, against the proof
    /// the upload started from. No proof (rules off) keeps D7 semantics:
    /// absence is "never created", any record reserves as before. A proof
    /// (rules on) only allows the proven incarnation: a different one, or
    /// absence, means the session was deleted or replaced between the proof
    /// and this first part (M2).
    fn proven_capture(
        &self,
        captured: Option<(
            DateTime<Utc>,
            harnx_runtime::nats_session_metadata::SessionMetadataStore,
        )>,
    ) -> SessionReservation {
        match capture_outcome(self.proven_incarnation, captured) {
            CaptureOutcome::Reserved((created_at, metadata)) => SessionReservation::Reserved {
                created_at,
                metadata: Box::new(metadata),
            },
            CaptureOutcome::Unreserved => SessionReservation::Unreserved,
            CaptureOutcome::Gone => SessionReservation::Gone,
        }
    }
}

/// What a capture read means against the upload's proof, without the store
/// handle so the deleted/replaced/recreated decision is unit-testable
/// without a broker. `Reserved` carries the captured record back out, so
/// the decision and the reservation are one step.
enum CaptureOutcome<T> {
    /// The captured incarnation may be addressed; recheck later parts.
    Reserved(T),
    /// No proof (rules off) and no record: never created (D7); the upload
    /// proceeds unrechecked.
    Unreserved,
    /// A proof (rules on) that the capture no longer matches: the session
    /// was deleted or replaced between the proof and this part.
    Gone,
}

/// The capture decision: without a proof any record reserves and absence is
/// D7 "never created"; with a proof only the proven incarnation reserves.
fn capture_outcome<T>(
    proven: Option<DateTime<Utc>>,
    captured: Option<(DateTime<Utc>, T)>,
) -> CaptureOutcome<(DateTime<Utc>, T)> {
    match (proven, captured) {
        (None, None) => CaptureOutcome::Unreserved,
        (None, Some(record)) => CaptureOutcome::Reserved(record),
        (Some(_), Some(record)) if Some(record.0) == proven => CaptureOutcome::Reserved(record),
        (Some(_), _) => CaptureOutcome::Gone,
    }
}

/// Confirmed absence, or a newer incarnation under the same session id.
/// Either way this upload addressed a session that no longer exists; the
/// version this put created must not outlive it — remove exactly that
/// version. A dedup hit owns nothing; its object was created by a live
/// upload of some incarnation and must stay. Cleanup is best-effort: the
/// caller already gets no ref, and the orphan is invisible to sessions.
///
/// Known residual race, accepted as best-effort per the plan note: a
/// concurrent upload of the same content (same session id, same hash)
/// can dedup-hit our just-written NUID before our recheck fails, so the
/// version is shared. If that upload also had a reserved incarnation,
/// its own recheck fails too and no ref is handed out; if it captured
/// unreserved (D7) or after a recreate, it may return a ref into the
/// version this purge removes — damage bounded to a same-CID upload
/// racing a session delete inside the put-ack-to-recheck window.
async fn purge_version_of_gone_session(
    bucket: &AttachmentsBucket,
    url: &CidUrl,
    outcome: &harnx_blob_store::media::MediaPut,
) {
    if let harnx_blob_store::media::MediaPut::Written { nuid } = outcome {
        if let Err(error) = harnx_blob_store::media::purge_media_version(bucket, url, nuid).await {
            log::error!(
                "failed to remove the upload written for a deleted or replaced session: {}",
                sanitized_error_chain(&error)
            );
        }
    }
}

/// Object-store failures must not surface as the route's default 400, and the
/// response must carry no refs. Broker-unavailability failures are 503 (the
/// store is currently unreachable); any other store failure is 500. Both keep
/// the `{"error": ...}` body shape the web UI reads, and neither body echoes
/// the object key or the internal failure chain: those stay in the log line,
/// which is what correlates a failed upload with its cause. The log line
/// renders the chain with NATS URL credentials redacted, so a cluster URL
/// with userinfo cannot reach the logs through a context message.
pub(crate) fn attachment_store_error(error: anyhow::Error) -> AttachmentUploadError {
    log::error!(
        "failed to store uploaded attachment in the NATS object store: {}",
        sanitized_error_chain(&error)
    );
    if is_nats_unavailable(&error) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "attachment object store is unavailable"}),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "failed to store attachment in the object store"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{capture_outcome, CaptureOutcome};
    use chrono::{DateTime, TimeZone, Utc};

    fn moment() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn other_moment() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_001, 0).unwrap()
    }

    /// The unit-test stand-in for the captured metadata record: any
    /// payload rides through the same decision.
    fn captured(incarnation: DateTime<Utc>) -> Option<(DateTime<Utc>, ())> {
        Some((incarnation, ()))
    }

    #[test]
    fn capture_without_proof_keeps_d7_semantics() {
        // Rules off: absence is "never created", any record reserves.
        assert!(matches!(
            capture_outcome::<()>(None, None),
            CaptureOutcome::Unreserved
        ));
        assert!(matches!(
            capture_outcome(None, captured(moment())),
            CaptureOutcome::Reserved((_, ()))
        ));
    }

    #[test]
    fn capture_with_proof_reserves_only_the_proven_incarnation() {
        let proven = moment();
        // The proven incarnation reserves.
        assert!(matches!(
            capture_outcome(Some(proven), captured(proven)),
            CaptureOutcome::Reserved((captured_at, ())) if captured_at == proven
        ));
        // Deleted between the proof and the first part.
        assert!(matches!(
            capture_outcome::<()>(Some(proven), None),
            CaptureOutcome::Gone
        ));
        // Replaced with a new incarnation between the proof and the part.
        assert!(matches!(
            capture_outcome(Some(proven), captured(other_moment())),
            CaptureOutcome::Gone
        ));
    }
}
