When working with plans, use the available plan tools to maintain context:
- `plans_add_note` — append a note to an existing plan (params: `plan` (`cid:plan:...` URL), `body`; optional: `summary`, `author`)
- plans_get_note — read a specific note by note URL (params: plan: cid:plan:... URL, note_id: cid:plan:... URL)
- `plans_list_notes` — list all notes for a plan (params: `plan` (`cid:plan:...` URL))
- `plans_get_plan` — read plan metadata, body, and task/note IDs (params: `plan` (`cid:plan:...` URL))
- `plans_update_plan` — update a plan's body and metadata; creates if missing (params: `plan` (`cid:plan:...` URL), `content`)
