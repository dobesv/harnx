# harnx-plans-tools

`harnx-plans-tools` manages plans, tasks, and notes in NATS JetStream KV. Every plan is addressed by a canonical `cid:plan:` URL owned by the session that created it.

Documents remain human-readable markdown with YAML front matter. They are stored in the `harnx_plans` bucket under these keys:

- `plan/<owner>/<slug>/plan`
- `plan/<owner>/<slug>/tasks/<task-id>`
- `plan/<owner>/<slug>/notes/<note-id>`

## Run

```yaml
command: harnx-plans-tools
description: NATS-backed plan/task/note management
```

The native server reads standard `HARNX_NATS_*` connection variables. `--name <NAME>` overrides the registered toolset name. `--mcp-stdio` and `--mcp-http` use the shared toolset-server transport options.

## URL parameters

`add_plan` takes a human name, slugifies it, and returns a URL such as:

```text
cid:plan:pantheon%2Fatlas/armDRA/nats-attachments
```

All other plan parameters require that full URL. Task get/update/delete operations require full task URLs. Note tools accept either a bare note ID or full note URL belonging to the supplied plan; `add_note.id` accepts both forms too:

```text
cid:plan:pantheon%2Fatlas/armDRA/nats-attachments/tasks/storage
cid:plan:pantheon%2Fatlas/armDRA/nats-attachments/notes/design
```

Task dependencies are task URLs. Bare plan names and bare task IDs are rejected. Note IDs use `[a-z0-9-]` and must contain 1–64 characters.

## Plan metadata

`add_plan` and `update_plan` store optional issue metadata in the plan's YAML front matter:

- `github_issue`: numeric GitHub issue number. Pair it with `github_owner_repo` (`owner/repo`) to identify the issue's repository. A repository isn't required by validation, so older records and callers that supply only an issue number still work.
- `external_task_url`: absolute `http://` or `https://` task URL for any issue tracker, including Jira, Linear, and GitHub. URLs must have a host; surrounding whitespace is trimmed. Invalid URLs and other schemes are rejected without fetching the URL.

Both fields can be set or changed on an existing plan, or stored when `update_plan` creates a missing plan owned by the caller. This is metadata only; tools don't sync with trackers or create GitHub issue relationships.

Omitted or `null` fields leave existing values unchanged. `github_issue: 0` and blank `external_task_url` values also count as omitted. `get_plan` and `list_plans` return `github_issue` and `external_task_url`, with `null` when unset.

Arguments appear in tool-call summaries and may be visible in commit bodies. Avoid storing URLs containing credentials or secrets.

Legacy stored `parent_issue` values load as `github_issue` and are written under the new name on the next update. Older callers can still send `parent_issue` as an alias, but it isn't advertised in tool schemas or returned in responses. Send only one of `github_issue` and `parent_issue`; providing both is rejected as a duplicate field. Documents without issue metadata still load unchanged.

## List pagination and scope

`list_plans`, `list_tasks`, and `list_notes` return a JSON object instead of an array:

```json
{"items": [], "next_cursor": null}
```

All three accept `limit` (default 20, clamped to 1..100) and an optional opaque
`cursor`. Pass the previous `next_cursor` as `cursor` with the same scope and
filters until `next_cursor` is null. Results are sorted by storage key, not by
creation time. Cursors are key-based, so deleting an earlier item doesn't shift
later pages. Listings aren't snapshots: concurrent inserts before the cursor
won't appear on subsequent pages.

`list_plans` defaults to the caller's session when session identity is available.
Use `owner` (a 64-character session key) to select another session, or
`all_sessions: true` to list every session. Without caller identity or an explicit
owner, it lists every session. Combining `owner` and `all_sessions: true` is an
error. Listing leaves session activity alone.

`list_tasks` and `list_notes` require a full plan URL and list only that plan's
children. Note bodies in `list_notes` are previews capped at 200 characters;
use `get_note` with an item's full note URL to read its complete body. Task
status (`filter`, default `open`, or `all` to disable) and `tag` filters apply
**within each key page**. This bounds document reads to `limit`,
but can return short or empty `items` with a non-null `next_cursor`. Keep paging
even after an empty page.

Only page document bodies are fetched, with at most 16 concurrent reads. Plan
counts come from live task/note keys, without reading their bodies. Key listing
still scans bucket-wide **headers only**: current slash-delimited KV keys occupy
one NATS subject token, so NATS's dot-token wildcards can't filter their prefixes.
Server-side prefix filtering requires a key migration or a separate index.
Deleted and purged keys are excluded; concurrent deletions can shorten a page.

## Tools

- Plans: `list_plans`, `add_plan`, `get_plan`, `update_plan`, `delete_plan`
- Tasks: `list_tasks`, `add_task`, `get_task`, `update_task`, `delete_task`
- Notes: `list_notes`, `add_note`, `get_note`, `update_note`, `delete_note`

Tool responses include markdown cross-links (`[title](url)`) and MCP `ContentBlock::resource_link` entries with MIME `text/markdown; charset=utf-8` for the plan index and affected item, while preserving diff blocks for mutations.

Writes use revision compare-and-set and retry conflicts up to five times. Writes touch owner session activity so active plans follow session retention instead of a separate filesystem cleanup policy. Reads don't, so a session whose plans are only read still expires.
