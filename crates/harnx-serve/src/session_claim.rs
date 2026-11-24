//! Client-chosen session IDs under access rules: the deferred claim path.
//!
//! When the session guard finds no canonical owner record, it no longer
//! answers for the request itself. For exactly `POST
//! /v1/agents/{agent}/sessions/{session}` with a claimable ID it defers the
//! decision ([`DeferredSessionClaim`]) to the item handler, which runs the
//! classification of plan design step 3 before any dispatch, resume
//! routing, actor lookup or spawn, actor command, or SSE subscribe, then
//! reserves the ID (step 4), authorizes the record the broker returned, and
//! continues through the normal existing-session path. Every non-creating
//! outcome hides behind the guard's 404; only the prompt-scope 403 and
//! broker-outage 503 differ (plan decisions D2, D5).

use anyhow::{anyhow, Result};
use harnx_core::access_rules::AccessRules;
use hyper::body::Incoming;

use crate::session_actor::ResolvedAgentTarget;
use crate::{Server, ERROR_STATUS_MARKER};

/// Request extension the guard inserts when the canonical owner record is
/// absent and the request may qualify for implicit creation. Only
/// [`crate::handle_agent_session`] reads it; the carried session ID is the
/// percent-decoded, claim-validated local ID every later step uses, so
/// reservation, authorization, and admission all target the exact requested
/// session. The body is what the guard's bounded candidate read already
/// collected (see [`read_candidate_body`]); the request's own body stream is
/// an empty replacement by then, so the claim path never re-reads it.
#[derive(Clone, Debug)]
pub(crate) struct DeferredSessionClaim {
    pub(crate) session: String,
    pub(crate) body: bytes::Bytes,
}

/// The session ID contract for a deferred claim (plan design step 1): a
/// safe path segment that is non-empty, carries no leading or trailing
/// whitespace, and satisfies the runtime's `SessionRef` session-id
/// validation (review 4677217455). Admission builds a `SessionRef` whose
/// constructor refuses IDs under 6 characters or outside `[A-Za-z0-9_-]`,
/// and `NatsSession::session_ref()` panics on what it refuses — so an ID
/// the runtime would not accept can never be deferred, let alone
/// reserved: it stays behind the guard's 404 before any metadata write,
/// instead of reserving an ID that stays unusable. The runtime's own
/// validation is reused (`SessionRef::new` with no agent validates
/// exactly the session id), never copied.
pub(crate) fn claimable_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.trim() == session
        && harnx_core::cid_url::SessionRef::new(None, session.to_string()).is_ok()
}

/// The ceiling for a candidate claim's request body: the crate's request
/// body cap, the same one multipart uploads enforce. A prompt JSON never
/// legitimately approaches it — bulk content travels as attachments.
pub(crate) const CLAIM_BODY_MAX_BYTES: usize = crate::MAX_UPLOAD_BYTES;

/// Whether the request's declared `Content-Length` alone exceeds the claim
/// body cap (review D3): a candidate this big is refused before a single
/// byte is buffered — the same guard 404 for a missing ID and a foreign one
/// (no missing-only 413), and no near-cap peak held in memory at all. The
/// declared length is only a precheck: an undeclared or lying length is
/// still caught by the cumulative cap in [`read_candidate_body`].
pub(crate) fn declared_body_exceeds_claim_cap(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > CLAIM_BODY_MAX_BYTES)
}

/// The guard's refused-candidate step (reviews C1/C3 and D3): refuse on
/// the declared length alone when it exceeds the cap, then run the same
/// bounded body read for a missing and a foreign owner — body-completion
/// timing and any interim `100 Continue` cannot tell the two apart, and the
/// cap is never a missing-only 413. A missing owner with a readable body
/// defers the decision to the item handler (plan design steps 2–5); every
/// other combination is the guard's 404.
pub(crate) async fn defer_or_refuse_candidate<B>(
    req: &mut http::Request<B>,
    owner_missing: bool,
) -> Option<bytes::Bytes>
where
    B: http_body::Body<Data = bytes::Bytes> + Unpin,
{
    if declared_body_exceeds_claim_cap(req.headers()) {
        return None;
    }
    let body = read_candidate_body(req.body_mut()).await?;
    owner_missing.then_some(body)
}

/// How long a candidate claim's body may take to arrive once the guard
/// starts reading it. A trickled body must not pin the connection.
pub(crate) const CLAIM_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The bounded body read every refused or deferred claim candidate runs —
/// a POST to exactly the item route with a claimable ID, whether the
/// record is missing or owned by someone else — before any owner-dependent
/// result, so the two cannot be told apart by body-completion timing or an
/// interim `100 Continue` (review C1). The bound is the crate's upload
/// cap plus a deadline, so the read is never an unbounded drain (review
/// C3). `None` — over the cap, past the deadline, or a read failure — is a
/// non-creating outcome: the guard's 404 on both sides, never a
/// missing-only 413.
///
/// Reads the body in place (`poll_frame`, no `into_data_stream`), so the
/// guard keeps the request — a deferred claim reuses the collected bytes
/// and never re-reads the drained stream.
pub(crate) async fn read_candidate_body<B>(body: &mut B) -> Option<bytes::Bytes>
where
    B: http_body::Body<Data = bytes::Bytes> + Unpin,
{
    tokio::time::timeout(claim_body_deadline(), async {
        // One growing buffer, not a chunk list plus a copy of it (review
        // D3): the cap is 20 MiB, so a Vec-of-chunks with a final `concat`
        // held ~2x that at peak.
        let mut buffer = bytes::BytesMut::new();
        loop {
            let frame =
                futures_util::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
                    .await;
            match frame {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(chunk) => {
                        if buffer.len() + chunk.len() > CLAIM_BODY_MAX_BYTES {
                            return None;
                        }
                        buffer.extend_from_slice(&chunk);
                    }
                    // A trailers frame is the end of the data.
                    Err(_) => break,
                },
                Some(Err(_)) => return None,
                None => break,
            }
        }
        Some(buffer.freeze())
    })
    .await
    .unwrap_or(None)
}

/// The deadline for [`read_candidate_body`]. Production uses the constant;
/// in-crate tests can tighten it to keep the deadline cases fast.
fn claim_body_deadline() -> std::time::Duration {
    #[cfg(test)]
    {
        let millis = TEST_CLAIM_BODY_DEADLINE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            return std::time::Duration::from_millis(millis);
        }
    }
    CLAIM_BODY_READ_TIMEOUT
}

/// Test-only override for the candidate body deadline, in milliseconds.
/// Zero means the production constant.
#[cfg(test)]
pub(crate) static TEST_CLAIM_BODY_DEADLINE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The guard's 404: the same `Not Found` error the session guard produces,
/// so the deferred path's refusals render byte-identically to today's.
pub(crate) fn guard_not_found() -> anyhow::Error {
    anyhow!("Not Found")
}

/// The scope refusal the session collection route uses (R5, decision D2):
/// same message, same error shape, nothing created.
pub(crate) fn creation_scope_denied() -> anyhow::Error {
    anyhow!("session creation requires prompt scope{ERROR_STATUS_MARKER}403")
}

/// The shared opening of a deferred claim, independent of representation:
/// the access context the continuation carries (so the preflight verdict is
/// reused, design step 5), the split request parts, and the collected body
/// (row b).
pub(crate) struct OpenedClaim<'a> {
    pub(crate) access: crate::ag_ui_rpc::RequestAccess<'a>,
    pub(crate) parts: http::request::Parts,
    pub(crate) body: bytes::Bytes,
    pub(crate) web_base_url: Option<String>,
}

/// What every representation's claim steps share: the access rules and
/// trusted identity the guard resolved, and the agent-scoped config the
/// reservation stamps — one parameter instead of three.
pub(crate) struct ClaimContext<'a> {
    pub(crate) rules: &'a AccessRules,
    pub(crate) identity: &'a crate::request_identity::RequestIdentity,
    pub(crate) scoped: &'a harnx_runtime::config::GlobalConfig,
}

// qual:allow(coupling, oi) reason: "The deferred claim path extends Server by concern, the way session_access.rs and agent_access.rs already do."
impl Server {
    /// The deferred-claim entry point, called by the item handler instead
    /// of the normal negotiation dispatch. Runs plan design step 3's
    /// classification rows in order; first match decides.
    pub(crate) async fn handle_deferred_claim(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        claim: DeferredSessionClaim,
        scoped: &harnx_runtime::config::GlobalConfig,
    ) -> Result<crate::ag_ui::AppResponse> {
        // A claim only exists with access rules on; treat any other state as
        // the guard's 404 rather than trusting the extension alone.
        let Some((rules, identity)) = self.access(&req) else {
            return Err(guard_not_found());
        };
        let context = ClaimContext {
            rules,
            identity: &identity,
            scoped,
        };
        // Test seam: a test can arm the claim gate to park the request here,
        // between the guard's missing-owner read and every later step of the
        // claim — classification, preflight, the row-h reread and the row-i
        // reservation — forcing the creation races deterministically.
        self.session_claim_gate.wait().await;
        // Row a: only the two prompt-capable representations can create; a
        // failed negotiation stays hidden behind the guard's 404.
        match crate::negotiate_agents_route(
            req.method(),
            req.headers(),
            crate::AgentsRoute::Session,
        ) {
            Ok(crate::AgentsRepresentation::AgUiRpc) => {
                self.claim_via_rpc(req, target, &claim, &context).await
            }
            Ok(crate::AgentsRepresentation::AgUiSse) => {
                self.claim_via_sse(req, target, &claim, &context).await
            }
            _ => Err(guard_not_found()),
        }
    }

    /// JSON-RPC claim: classify (rows b–e), check scope and attachments
    /// (rows f–g), reserve and authorize (rows h–i), then continue through
    /// the normal RPC path with the preflight result carried.
    async fn claim_via_rpc(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        claim: &DeferredSessionClaim,
        context: &ClaimContext<'_>,
    ) -> Result<crate::ag_ui::AppResponse> {
        let session = claim.session.as_str();
        let opened = self
            .open_claim_request(req, &claim.body, context.rules, context.identity)
            .await?;
        // Rows b–e: parse, method, resume, and content. Anything that does
        // not qualify as a creating prompt stays hidden.
        let Some(params) = crate::ag_ui_rpc::rpc_prompt_claim(&opened.body) else {
            return Err(guard_not_found());
        };
        require_claim_scope(
            context.rules,
            &self.display_ref(target),
            context.identity.caller().view(),
        )?;
        claim_prompt_preflight(&opened.access, &params, target.cluster()).await?;
        self.reserve_and_authorize_claim(target, context.identity, session, context.scoped)
            .await?;
        self.continue_claim_rpc(opened, target, session, context.identity)
            .await
    }

    /// Continue an authorized claim through the normal existing-session RPC
    /// path. The carried preflight verdict keeps `handle_prompt` from
    /// re-checking the same refs after the session now exists (design
    /// step 5).
    async fn continue_claim_rpc(
        &self,
        opened: OpenedClaim<'_>,
        target: &ResolvedAgentTarget,
        session: &str,
        identity: &crate::request_identity::RequestIdentity,
    ) -> Result<crate::ag_ui::AppResponse> {
        let context = crate::ag_ui_rpc::RpcContext {
            config: &self.config,
            registry: &self.session_registry,
            persistence: crate::ag_ui_rpc::PersistenceKind::Nats,
            web_base_url: opened.web_base_url,
            user_id: identity.resolved_user_id().map(str::to_string),
            user_principals: identity.stampable_principals(),
            attachment_access: Some(opened.access),
        };
        crate::ag_ui_rpc::handle_ag_ui_rpc_bytes_for_target(
            crate::ag_ui_rpc::RpcRequest {
                method: opened.parts.method,
                target,
                session,
                body: opened.body,
            },
            context,
            // The preflight verdict is an internal parameter here (review
            // C-new-2): row g already authorized these refs pre-reservation.
            true,
        )
        .await
    }

    /// AG-UI SSE claim: classify (rows b–e), check scope (row f), reserve
    /// and authorize (rows h–i), then continue through the normal SSE run.
    /// The SSE path has no prompt validation of its own today, so row g has
    /// no step here (attachment validation on SSE stays out of scope).
    async fn claim_via_sse(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        claim: &DeferredSessionClaim,
        context: &ClaimContext<'_>,
    ) -> Result<crate::ag_ui::AppResponse> {
        let session = claim.session.as_str();
        let opened = self
            .open_claim_request(req, &claim.body, context.rules, context.identity)
            .await?;
        // Rows b, d, e: AG-UI parse errors, resume decisions, and a missing
        // new nonblank user message (against an empty snapshot) all
        // disqualify; the session does not exist, so nothing can be pending.
        if !crate::ag_ui::run_input_qualifies_as_creating_prompt(&opened.body) {
            return Err(guard_not_found());
        }
        require_claim_scope(
            context.rules,
            &self.display_ref(target),
            context.identity.caller().view(),
        )?;
        self.reserve_and_authorize_claim(target, context.identity, session, context.scoped)
            .await?;
        self.continue_claim_sse(opened, target, session, context.identity)
            .await
    }

    /// Continue an authorized claim through the normal SSE run path, now on
    /// an existing session. The SSE run request itself never needed the
    /// access context: row-g attachment authorization happens in the claim
    /// preflight before reservation, not in the run (review C12).
    async fn continue_claim_sse(
        &self,
        opened: OpenedClaim<'_>,
        target: &ResolvedAgentTarget,
        session: &str,
        identity: &crate::request_identity::RequestIdentity,
    ) -> Result<crate::ag_ui::AppResponse> {
        let request = crate::ag_ui::AgUiRunRequest {
            target,
            session,
            body: &opened.body,
            web_base_url: opened.web_base_url,
            user_id: identity.resolved_user_id().map(str::to_string),
            user_principals: identity.stampable_principals(),
        };
        crate::ag_ui::ag_ui_run_for_target(
            &self.config,
            &self.session_registry,
            request,
            self.shutdown.clone(),
        )
        .await
        .map_err(crate::ag_ui_error_to_anyhow)
    }

    /// Split the request and build the access context the continuation
    /// carries so the preflight verdict is reused rather than re-checked.
    /// The body is the guard's bounded candidate read, already collected
    /// before any owner-dependent result; the request's replaced body
    /// stream is discarded unread.
    async fn open_claim_request<'a>(
        &'a self,
        req: hyper::Request<Incoming>,
        body: &bytes::Bytes,
        rules: &'a AccessRules,
        identity: &crate::request_identity::RequestIdentity,
    ) -> Result<OpenedClaim<'a>> {
        let web_base_url = crate::web_url::inferred_base_url(req.headers(), req.uri());
        let access = crate::ag_ui_rpc::RequestAccess {
            rules,
            config: &self.config,
            identity: identity.clone(),
        };
        let (parts, _replaced_body) = req.into_parts();
        Ok(OpenedClaim {
            access,
            parts,
            body: body.clone(),
            web_base_url,
        })
    }
}

/// Row f: a creating prompt takes prompt scope, like the session collection
/// route. Admin alone is a visible 403 (R5, decision D2).
fn require_claim_scope(
    rules: &AccessRules,
    agent_ref: &str,
    caller: harnx_core::access_rules::CallerView<'_>,
) -> Result<()> {
    if rules.can_create_session(agent_ref, caller) {
        Ok(())
    } else {
        Err(creation_scope_denied())
    }
}

/// Row g: the attachment preflight runs once, here, before any reservation.
/// Denials, malformed refs and over-limit refs are pre-reservation
/// failures, hidden behind the guard's 404; a broker outage answers with
/// the 503-class refusal (decision D5).
async fn claim_prompt_preflight(
    access: &crate::ag_ui_rpc::RequestAccess<'_>,
    params: &crate::ag_ui_rpc::PromptParams,
    cluster: &str,
) -> Result<()> {
    match crate::ag_ui_rpc::check_prompt_attachments(access, params, cluster).await {
        crate::ag_ui_rpc::AttachmentGuard::Allow => Ok(()),
        crate::ag_ui_rpc::AttachmentGuard::Unavailable => {
            Err(crate::session_claim_reservation::session_metadata_unavailable())
        }
        _ => Err(guard_not_found()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A body whose data frames arrive one `Bytes` at a time — the shape
    /// hyper's identity and chunked decoders both hand the guard.
    struct FramesBody(std::vec::IntoIter<bytes::Bytes>);

    impl http_body::Body for FramesBody {
        type Data = bytes::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(
                self.get_mut()
                    .0
                    .next()
                    .map(|chunk| Ok(http_body::Frame::data(chunk))),
            )
        }
    }

    fn frames_body(lengths: &[usize]) -> FramesBody {
        FramesBody(
            lengths
                .iter()
                .map(|length| bytes::Bytes::from(vec![b'a'; *length]))
                .collect::<Vec<_>>()
                .into_iter(),
        )
    }

    /// Review D5: the cap is exact — `CLAIM_BODY_MAX_BYTES` is the last
    /// accepted length, and one byte more refuses, within a single frame
    /// and across a frame boundary alike.
    #[tokio::test]
    async fn claim_body_read_accepts_the_exact_cap_and_refuses_cap_plus_one() {
        let cap = CLAIM_BODY_MAX_BYTES;
        let mut body = frames_body(&[cap]);
        assert_eq!(
            read_candidate_body(&mut body)
                .await
                .expect("a body of exactly the cap")
                .len(),
            cap
        );
        let mut body = frames_body(&[cap / 2, cap / 2]);
        assert_eq!(
            read_candidate_body(&mut body)
                .await
                .expect("the cap across two frames")
                .len(),
            cap
        );
        let mut body = frames_body(&[cap / 2, cap / 2 + 1]);
        assert!(
            read_candidate_body(&mut body).await.is_none(),
            "one byte over the cap must refuse"
        );
    }

    /// Review D5: an empty body and a multi-frame body both read to the
    /// end — the empty case is a legitimate candidate (its parse decides
    /// later), and the chunked case must concatenate in place.
    #[tokio::test]
    async fn claim_body_read_accepts_empty_and_concatenates_chunked_bodies() {
        let mut body = frames_body(&[]);
        let read = read_candidate_body(&mut body)
            .await
            .expect("an empty body reads to the end");
        assert_eq!(&read[..], b"");

        let mut body = frames_body(&[2, 3]);
        let read = read_candidate_body(&mut body)
            .await
            .expect("a chunked body reads to the end");
        assert_eq!(&read[..], b"aaaaa");
    }

    /// Review D3: the declared-length precheck bounds the same cap the
    /// read enforces — `cap` passes, `cap + 1` refuses, and an absent or
    /// unparsable declaration stays a read instead of a refusal.
    #[test]
    fn claim_body_declared_length_precheck_bounds_the_cap() {
        let headers_with = |value: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CONTENT_LENGTH,
                value.parse::<http::HeaderValue>().expect("header value"),
            );
            headers
        };
        assert!(!declared_body_exceeds_claim_cap(&headers_with(
            &CLAIM_BODY_MAX_BYTES.to_string()
        )));
        assert!(declared_body_exceeds_claim_cap(&headers_with(
            &(CLAIM_BODY_MAX_BYTES + 1).to_string()
        )));
        assert!(!declared_body_exceeds_claim_cap(&http::HeaderMap::new()));
        assert!(!declared_body_exceeds_claim_cap(&headers_with("twelve")));
        assert!(!declared_body_exceeds_claim_cap(&headers_with("0")));
    }
}
