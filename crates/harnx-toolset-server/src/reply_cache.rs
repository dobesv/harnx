//! In-process payload cache for duplicate requests. It is a shortcut past the
//! journal, never an authority of its own: only replies the journal already
//! holds are kept here.
use super::*;
use sha2::{Digest, Sha256};

pub(super) type ReplyCache = Arc<Mutex<HashMap<String, ReplyCacheEntry>>>;
pub(super) type CachedReply = Arc<ToolReply>;

pub(super) enum ReplyCacheEntry {
    InProgress {
        reply: watch::Receiver<Option<CachedReply>>,
    },
    Complete {
        created: Instant,
        reply: CachedReply,
    },
}

pub(super) enum CacheReservation {
    Execute(watch::Sender<Option<CachedReply>>),
    Wait(watch::Receiver<Option<CachedReply>>),
    Complete(CachedReply),
    Full,
}

pub(super) async fn reserve_cache_entry(cache: &ReplyCache, key: &str) -> CacheReservation {
    let mut cache = cache.lock().await;
    remove_expired_replies(&mut cache, Instant::now());
    if let Some(entry) = cache.get(key) {
        return match entry {
            ReplyCacheEntry::InProgress { reply, .. } => CacheReservation::Wait(reply.clone()),
            ReplyCacheEntry::Complete { reply, .. } => CacheReservation::Complete(reply.clone()),
        };
    }
    if cache.len() >= IDEMPOTENCY_CACHE_MAX_ENTRIES {
        evict_oldest_completed_reply(&mut cache);
    }
    if cache.len() >= IDEMPOTENCY_CACHE_MAX_ENTRIES {
        return CacheReservation::Full;
    }
    let (completion, reply) = watch::channel(None);
    cache.insert(key.to_string(), ReplyCacheEntry::InProgress { reply });
    CacheReservation::Execute(completion)
}

fn remove_expired_replies(cache: &mut HashMap<String, ReplyCacheEntry>, now: Instant) {
    cache.retain(|_, entry| match entry {
        ReplyCacheEntry::InProgress { .. } => true,
        ReplyCacheEntry::Complete { created, .. } => {
            now.duration_since(*created) < IDEMPOTENCY_CACHE_TTL
        }
    });
}

fn evict_oldest_completed_reply(cache: &mut HashMap<String, ReplyCacheEntry>) {
    let oldest = cache
        .iter()
        .filter_map(|(key, entry)| match entry {
            ReplyCacheEntry::Complete { created, .. } => Some((key.clone(), *created)),
            ReplyCacheEntry::InProgress { .. } => None,
        })
        .min_by_key(|(_, created)| *created)
        .map(|(key, _)| key);
    if let Some(key) = oldest {
        cache.remove(&key);
    }
}

pub(super) async fn wait_for_cached_reply(
    mut reply: watch::Receiver<Option<CachedReply>>,
) -> Result<CachedReply> {
    if reply.borrow().is_none() {
        reply
            .changed()
            .await
            .context("original idempotent tool request ended without a reply")?;
    }
    let cached = reply.borrow().clone();
    cached.context("original idempotent tool request ended without a reply")
}

pub(super) async fn complete_cache_entry(
    cache: &ReplyCache,
    key: String,
    reply: CachedReply,
    completion: watch::Sender<Option<CachedReply>>,
) {
    let mut cache = cache.lock().await;
    if reply.result.is_ok() {
        cache.insert(
            key,
            ReplyCacheEntry::Complete {
                created: Instant::now(),
                reply: reply.clone(),
            },
        );
    } else {
        // A failure may have been rejected before it was journaled, so let a
        // duplicate ask the journal again instead of serving it from memory.
        cache.remove(&key);
    }
    let _ = completion.send(Some(reply));
}

/// Two requests share a cached reply only when they are the same call of the
/// same session carrying the same arguments.
pub(super) fn cache_key(request: &ToolRequest) -> Result<String> {
    let session = request
        .parent_session_id
        .as_deref()
        .unwrap_or(&request.call_id);
    let digest = digest(&serde_json::to_vec(request).context("encode tool request")?);
    Ok(format!("{session}/{}/{digest}", request.call_id))
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut hex, "{byte:02x}").expect("writing into a String");
    }
    hex
}

pub(super) async fn serve_cached(
    context: &ToolRequestContext,
    request: &ToolRequest,
    subject: harnx_nats_common::rpc::ReplyTarget,
    saved: CachedReply,
) -> Result<()> {
    let mut reply = (*saved).clone();
    finalize_execution_context(context, request, &mut reply);
    publish_reply(&context.client, subject, &reply).await
}
