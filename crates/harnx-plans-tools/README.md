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

All other plan parameters require that full URL. Task and note get/update/delete operations also require full item URLs:

```text
cid:plan:pantheon%2Fatlas/armDRA/nats-attachments/tasks/storage
cid:plan:pantheon%2Fatlas/armDRA/nats-attachments/notes/design
```

Task dependencies are task URLs. Bare plan names and bare item IDs are rejected.

## Plan metadata

`add_plan` and `update_plan` store optional issue metadata in the plan's YAML front matter:

- `github_issue`: numeric GitHub issue number. Pair it with `github_owner_repo` (`owner/repo`) to identify the issue's repository. A repository isn't required by validation, so older records and callers that supply only an issue number still work.
- `external_task_url`: absolute `http://` or `https://` task URL for any issue tracker, including Jira, Linear, and GitHub. URLs must have a host; surrounding whitespace is trimmed. Invalid URLs and other schemes are rejected without fetching the URL.

Both fields can be set or changed on an existing plan, or stored when `update_plan` creates a missing plan owned by the caller. This is metadata only; tools don't sync with trackers or create GitHub issue relationships.

Omitted or `null` fields leave existing values unchanged. `github_issue: 0` and blank `external_task_url` values also count as omitted. `get_plan` and `list_plans` return `github_issue` and `external_task_url`, with `null` when unset.

Arguments appear in tool-call summaries and may be visible in commit bodies. Avoid storing URLs containing credentials or secrets.

Legacy stored `parent_issue` values load as `github_issue` and are written under the new name on the next update. Older callers can still send `parent_issue` as an alias, but it isn't advertised in tool schemas or returned in responses. Send only one of `github_issue` and `parent_issue`; providing both is rejected as a duplicate field. Documents without issue metadata still load unchanged.

## Tools

- Plans: `list_plans`, `add_plan`, `get_plan`, `update_plan`, `delete_plan`
- Tasks: `list_tasks`, `add_task`, `get_task`, `update_task`, `delete_task`
- Notes: `list_notes`, `add_note`, `get_note`, `update_note`, `delete_note`

Tool responses include markdown cross-links (`[title](url)`) and MCP `ContentBlock::resource_link` entries with MIME `text/markdown; charset=utf-8` for the plan index and affected item, while preserving diff blocks for mutations.

Writes use revision compare-and-set and retry conflicts up to five times. Reads and writes touch owner session activity so active plans follow session retention instead of a separate filesystem cleanup policy.
