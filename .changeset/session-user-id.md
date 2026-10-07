---
harnx: minor
---

feat(session): record user identity with each session (#2295)

Add user identity tracking to canonical session metadata:
- Store an opaque user identity string in session property `user_id` when a session is created. Identity is immutable and never overwritten on subsequent turns.
- Configure default user identity globally via `user_id` in `config.yaml` or `HARNX_USER_ID` (including `.env`).
- Configure cluster-level default user identity via `user_id` in `nats_servers/<cluster>.yaml`.
- Support request-derived user identity in `harnx-serve` via `serve_user_id_sources` in `config.yaml`, `HARNX_SERVE_USER_ID_SOURCES`, or repeated `--user-id-source` CLI flags.
- Identity sources support `header:NAME`, `cookie:NAME`, and bare `NAME` syntax with first-present-wins evaluation and fail-closed validation (HTTP 401 on empty or malformed values).
- Expose `user_id` in `harnx-serve` session summaries and Web UI `SessionRef`.
- Handoff sessions inherit the source session's `user_id`, like sub-agent sessions.
- `session/cancel` on a session that was never prompted is now a no-op, and `session/compact` on one returns 404 (JSON-RPC `-32001`), so control calls can't create a session without its request identity.
