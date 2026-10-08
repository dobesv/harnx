---
harnx: minor
---

feat(access): optional access rules for harnx-serve and harnx-a2a-server (#2316)

Add optional identity-based access control rules (`access.yaml`) gating agents, sessions, and attachments in `harnx-serve` and `harnx-a2a-server`:
- Load rules from `<config_dir>/access.yaml` or an explicit path via `--access-rules <PATH>` / `HARNX_ACCESS_RULES`. When no file is present and no flag/env is supplied, access control is disabled and existing behavior is preserved.
- When access rules are active, require trusted caller identity sources (`serve_user_id_sources` / `--user-id-source` in `harnx-serve`, `--user-id-header` in `harnx-a2a-server`); startup fails closed if rules are active without configured identity sources.
- Protected endpoints (`/v1/agents*` and `/v1/cid/*` in `harnx-serve`, all export and discovery routes in `harnx-a2a-server`) return HTTP 401 Unauthorized when a request lacks caller identity.
- Rule matching uses case-sensitive whole-string globs across agent references and user identity strings (where `*` spans commas, supporting full DN strings). Rules define `prompt` and `admin` scopes (omitted scopes default to `[prompt]`). Matching scopes union across rules.
- Agents without granted scope are hidden from listings and return 404 (or JSON-RPC `-32001` in A2A).
- Gated session access: callers with `prompt` scope can create sessions and access sessions they own; callers with `admin` scope can view, operate, and compact any session (including legacy sessions without owner `user_id`). Creating a new session or context requires `prompt` scope; callers with only `admin` receive HTTP 403 in `harnx-serve` or JSON-RPC `-32010` in `harnx-a2a-server`.
- In `harnx-serve`, callers must reserve a session ID via `POST /v1/agents/{agent}/sessions` before prompting when access rules are active.
- Gated CID retrieval (`/v1/cid/*`): attachments and plans are authorized against the owning session's access rules. When rules are enabled, responses emit `Cache-Control: private, no-store` (operators should purge shared caches when turning rules on).
- Agent reference matching: `harnx-serve` matches against display references (`agent` on the default cluster, `agent@cluster` otherwise); `harnx-a2a-server` matches against internal agent references (`agent` or `agent@cluster`), not public export aliases.
