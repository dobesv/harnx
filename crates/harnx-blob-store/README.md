# harnx-blob-store

`harnx-blob-store` is a standalone crate providing NATS JetStream storage operations for media attachments and plan documents identified by canonical `cid:` URLs.

## Modules

- **`media`**: JetStream Object Store operations for attachments. Handles uploading (`put_media`), retrieval (`get_media`), bucket provisioning (`ensure_attachments_bucket`), and prefix cleanup (`delete_media_prefix`). Objects live in the `harnx_attachments` bucket under `media/<owner>/<hash>`.
- **`plans`**: JetStream Key-Value store operations for plans. Handles bucket initialization (`ensure_plans_bucket`), plan index, task, and note serialization, revision compare-and-set updates, and markdown rendering (`render`) with cross-links. Documents live in the `harnx_plans` bucket under `plan/<owner>/<slug>/...`.
- **`activity`**: Rate-limited session activity renewal (`touch_activity`, `ActivityGuard`). Updates `SessionActivity.last_activity_at` in the `harnx_sessions` KV bucket when media or plans are accessed, debounced in-process to at most one write per hour per session owner.
- **`delete`**: Owner cleanup cascading (`delete_owner`). Purges both `media/<owner>/` objects in `harnx_attachments` and `plan/<owner>/` keys in `harnx_plans` when a session is deleted or expired by garbage collection.
- **`resolve`**: Unified resolution (`resolve`, `ResolvedBlob`). Resolves canonical `cid:media:` and `cid:plan:` URLs into content bytes, MIME types, cache ETags, and immutability flags, automatically touching the owner's session activity timestamp.

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
