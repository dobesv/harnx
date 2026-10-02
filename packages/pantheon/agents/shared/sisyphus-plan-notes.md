When working with plans, use the available plan tools to maintain context:
- `plans_add_plan` — create a plan from a human name and use its returned `cid:plan:...` URL; no plan URL is needed to create it (optional metadata: `github_issue`, `github_owner_repo`, `external_task_url`)
- `plans_add_note` — append a note to an existing plan (params: `plan` (`cid:plan:...` URL), `body`; optional: `summary`, `author`)
- plans_get_note — read a specific note by note URL (params: plan: cid:plan:... URL, note_id: cid:plan:... URL)
- `plans_list_notes` — list all notes for a plan (params: `plan` (`cid:plan:...` URL))
- `plans_get_plan` — read plan metadata, body, and task/note IDs (params: `plan` (`cid:plan:...` URL))
- `plans_update_plan` — update an existing plan's body and metadata (params: `plan` (the returned `cid:plan:...` URL), `replace_content`/`content`, `github_issue`, `github_owner_repo`, `external_task_url`)
