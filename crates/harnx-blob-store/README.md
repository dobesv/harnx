# harnx-blob-store

`harnx-blob-store` is a standalone crate providing NATS JetStream storage operations for media attachments and plan documents identified by canonical `cid:` URLs.

## Modules

- **`media`**: JetStream Object Store operations for attachments. Handles uploading (`put_media`), retrieval (`get_media`), bucket provisioning (`ensure_attachments_bucket`), and prefix cleanup (`delete_media_prefix`). Objects live in the `harnx_attachments` bucket under `media/<owner>/<hash>`. `AttachmentsBucket` pairs the object store with its stream handle so writes and leader reads share one JetStream context. `get_media` is a direct read that a lagging replica can answer with "not found" for an acknowledged write; `leader_media_info` (raw `STREAM.MSG.GET` on the metadata subject, tombstones returned visibly) and `read_media_from_leader` (next-by-subject raw gets on the chunk subject, no consumers, all chunks and retries bounded by one 30-second deadline) confirm misses against the stream leader. `put_media` dedup trusts only that leader lookup and skips only live objects — a tombstone or stale follower entry never suppresses a write, and a failed probe falls through to the write (dedup is best-effort). `put_media_versioned` reports the outcome instead: `MediaPut::DedupHit` (existing object, caller owns nothing) or `MediaPut::Written { nuid }` identifying exactly this attempt's chunks and metadata version. `purge_media_version` removes one such written version — its metadata message first (only while it still names that NUID, so a newer same-CID version is never touched), then that NUID's chunks via a subject-filtered purge — never call it for a dedup hit. `attachments_stream_name`/`attachments_stream_missing` expose the stream name and the typed missing-stream check for callers probing the bucket without duplicating the `OBJ_` scheme.
- **`plans`**: JetStream Key-Value store operations for plans. Handles bucket initialization (`ensure_plans_bucket`), plan index, task, and note serialization, revision compare-and-set updates, and markdown rendering (`render`) with cross-links. Documents live in the `harnx_plans` bucket under `plan/<owner>/<slug>/...`.
- **`activity`**: Rate-limited session activity renewal (`touch_activity`, `ActivityGuard`). Updates `SessionActivity.last_activity_at` in the `harnx_sessions` KV bucket when media or plans are written, debounced in-process to at most one write per hour per session owner.
- **`delete`**: Owner cleanup cascading (`delete_owner`). Purges both `media/<owner>/` objects in `harnx_attachments` and `plan/<owner>/` keys in `harnx_plans` when a session is deleted or expired by garbage collection.
- **`resolve`**: Unified resolution (`resolve`, `resolve_bounded`, `ResolvedBlob`). Resolves canonical `cid:media:` and `cid:plan:` URLs into content bytes, MIME types, cache ETags, and immutability flags. A read leaves the owner's session activity alone. `resolve_bounded(jetstream, url, max_bytes)` rejects oversized media from metadata before downloading and reads at most `max_bytes + 1` bytes. Plans are checked after Markdown rendering. Size failures return a downcastable `BlobTooLarge { size_bytes, max_bytes }` error; `size_bytes` is a lower bound when a read stops early. Existing `resolve` has no size limit.

## Layering constraints

Tool servers depend on `harnx-blob-store` to perform NATS object and KV operations without linking the full execution runtime.

The crate depends strictly on:
- `harnx-core` (pure models, no I/O)
- `harnx-nats-common` (connection helpers)
- `async-nats` (NATS client)

`harnx-blob-store` must not depend on `harnx-runtime` or `harnx-toolset-server`.

## Verification

To verify that layering constraints are preserved:

```sh
cargo tree -p harnx-blob-store -i harnx-runtime
cargo tree -p harnx-blob-store -i harnx-toolset-server
```

Both commands should return an error indicating that the specified packages do not match any dependencies in the tree. You can inspect the entire dependency tree with:

```sh
cargo tree -p harnx-blob-store
```
