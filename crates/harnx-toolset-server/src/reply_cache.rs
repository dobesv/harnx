//! In-process payload cache; positive hits still require durable consumption CAS.
use super::*;
use invocation_journal::CommittedReply;

pub(super) type ReplyCache = Arc<Mutex<HashMap<String, ReplyCacheEntry>>>;

pub(super) enum ReplyCacheEntry {
    InProgress {
        reply: watch::Receiver<Option<CachedReply>>,
    },
    Complete {
        created: Instant,
        reply: Arc<CommittedReply>,
    },
}

pub(super) enum CacheReservation {
    Execute(watch::Sender<Option<CachedReply>>),
    Wait(watch::Receiver<Option<CachedReply>>),
    Complete(Arc<CommittedReply>),
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

pub(super) type CachedReply = Result<Arc<CommittedReply>, ToolErrorPayload>;

pub(super) async fn complete_cache_entry(
    cache: &ReplyCache,
    key: String,
    saved: CachedReply,
    completion: watch::Sender<Option<CachedReply>>,
) {
    let mut cache = cache.lock().await;
    match &saved {
        Ok(reply) => {
            cache.insert(
                key,
                ReplyCacheEntry::Complete {
                    created: Instant::now(),
                    reply: reply.clone(),
                },
            );
        }
        Err(_) => {
            cache.remove(&key);
        }
    }
    let _ = completion.send(Some(saved));
}

pub(super) fn cache_key(request: &ToolRequest, idempotency_key: &str) -> Result<String> {
    Ok(serde_json::to_string(&(
        reply_fence::identity(request)?,
        &request.call_id,
        idempotency_key,
    ))?)
}

pub(super) async fn cache_completion(
    context: &ToolRequestContext,
    request: &ToolRequest,
    reply: ToolReply,
) -> CachedReply {
    let result = async {
        reply_fence::check_stop(&context.execution_store, reply_fence::identity(request)?).await?;
        let Some(saved) = context.journal.committed_reply(request).await? else {
            anyhow::ensure!(reply.result.is_err(), "success has no committed proof");
            return Ok(None);
        };
        if saved.reply != reply {
            return Ok(None);
        }
        reply_fence::consume(&context.execution_store, request, &saved).await?;
        Ok::<_, anyhow::Error>(Some(saved))
    }
    .await;
    match result {
        Ok(Some(saved)) => Ok(Arc::new(saved)),
        Ok(None) => match reply.result {
            Err(error) => Err(error),
            Ok(_) => Err(ToolErrorPayload::Fatal(
                "tool reply differs from committed proof".into(),
            )),
        },
        Err(error) => Err(map_invoke_error(reply_fence::invoke_error(error))),
    }
}

pub(super) async fn serve_cached(
    context: &ToolRequestContext,
    request: &ToolRequest,
    subject: harnx_nats_common::rpc::ReplyTarget,
    saved: CachedReply,
) -> Result<()> {
    let result = match saved {
        Ok(saved) => reply_fence::consume(&context.execution_store, request, &saved).await,
        Err(error) => Ok(ToolReply {
            call_id: request.call_id.clone(),
            result: Err(error),
        }),
    };
    let mut reply = result.unwrap_or_else(|error| ToolReply {
        call_id: request.call_id.clone(),
        result: Err(map_invoke_error(reply_fence::invoke_error(error))),
    });
    finalize_execution_context(context, request, &mut reply);
    publish_reply(&context.client, subject, &reply).await
}

#[cfg(test)]
mod tests;
