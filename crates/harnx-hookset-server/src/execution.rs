//! Runs one hook invocation under a per-call cancellation token, so a control
//! message can interrupt it before it completes.
use super::*;
use harnx_core::hooks::HookPayload;
use harnx_toolset::InterruptedCall;

/// One hook call this server is currently running, so a control message can
/// find and cancel it.
#[derive(Clone)]
pub(super) struct ActiveHook {
    pub session_id: String,
    pub cancel: CancellationToken,
    cancellation_id: Arc<Mutex<Option<String>>>,
}

impl ActiveHook {
    /// Name the cancellation that stopped this call. The first one wins: a
    /// retry of the same stop must not relabel an interruption already decided.
    pub async fn set_cancellation_id(&self, id: &str) {
        let mut slot = self.cancellation_id.lock().await;
        slot.get_or_insert_with(|| id.to_owned());
    }
}

/// Hook calls this server is currently running, keyed by call ID, so a
/// control message can find and cancel one. There is no journal behind this:
/// a call ID absent here simply is not running.
pub(super) type HookRegistry = Arc<Mutex<HashMap<String, ActiveHook>>>;

pub(super) async fn handle(
    client: async_nats::Client,
    hook: Arc<dyn Hook>,
    active: HookRegistry,
    message: async_nats::Message,
) -> Result<()> {
    // Every request is answered, including one this server will not run. A
    // caller left waiting cannot tell a refusal from a lost message, and its
    // only recourse is the timeout its fail policy then acts on blindly.
    let response = match dispatch(hook, &active, &message).await {
        Ok(response) => response,
        Err(reason) => {
            log::warn!("refusing hook request: {reason:#}");
            crate::wire::refused_reply(&reason)
        }
    };
    if let Ok(reply) = harnx_nats_common::rpc::ReplyTarget::from_message(&message) {
        reply.send(&client, serde_json::to_vec(&response)?).await?;
    }
    Ok(())
}

async fn dispatch(
    hook: Arc<dyn Hook>,
    active: &HookRegistry,
    message: &async_nats::Message,
) -> Result<serde_json::Value> {
    let (session_id, call_id) = call_identity(message)?;
    let cancel = CancellationToken::new();
    let active_hook = ActiveHook {
        session_id,
        cancel: cancel.clone(),
        cancellation_id: Arc::default(),
    };
    active
        .lock()
        .await
        .insert(call_id.clone(), active_hook.clone());

    let response = run(hook, &message.payload, cancel, &active_hook).await;
    active.lock().await.remove(&call_id);
    response
}

/// The session and call a request belongs to. Without both, a control message
/// could never reach this call, so the request is refused rather than run
/// uncancellable.
fn call_identity(message: &async_nats::Message) -> Result<(String, String)> {
    let headers = message
        .headers
        .as_ref()
        .context("hook request missing headers")?;
    let session_id = headers
        .get(crate::wire::HOOK_SESSION_HEADER)
        .with_context(|| format!("{} header missing", crate::wire::HOOK_SESSION_HEADER))?
        .to_string();
    let call_id = headers
        .get(crate::wire::HOOK_CALL_HEADER)
        .with_context(|| format!("{} header missing", crate::wire::HOOK_CALL_HEADER))?
        .to_string();
    Ok((session_id, call_id))
}

async fn run(
    hook: Arc<dyn Hook>,
    payload: &[u8],
    cancel: CancellationToken,
    active_hook: &ActiveHook,
) -> Result<serde_json::Value> {
    let payload: HookPayload = serde_json::from_slice(payload)?;
    let future = hook.handle_hook(payload);
    tokio::pin!(future);
    tokio::select! {
        outcome = &mut future => Ok(crate::wire::completed_reply(serde_json::to_value(outcome)?)),
        _ = cancel.cancelled() => Ok(crate::wire::interrupted_reply(InterruptedCall {
            cancellation_id: active_hook.cancellation_id.lock().await.clone(),
            reason: "hook cancelled".into(),
        })),
    }
}
