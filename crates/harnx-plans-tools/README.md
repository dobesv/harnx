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

## Tools

- Plans: `list_plans`, `add_plan`, `get_plan`, `update_plan`, `delete_plan`
- Tasks: `list_tasks`, `add_task`, `get_task`, `update_task`, `delete_task`
- Notes: `list_notes`, `add_note`, `get_note`, `update_note`, `delete_note`

Tool responses include markdown cross-links (`[title](url)`) and MCP `ContentBlock::resource_link` entries with MIME `text/markdown; charset=utf-8` for the plan index and affected item, while preserving diff blocks for mutations.

Writes use revision compare-and-set and retry conflicts up to five times. Reads and writes touch owner session activity so active plans follow session retention instead of a separate filesystem cleanup policy.
