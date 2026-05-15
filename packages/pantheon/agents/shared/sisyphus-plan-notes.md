When working with plans, use the available plan tools to maintain context:
- `plans_add_note` — append a note to an existing plan (params: `plan`, `body`; optional: `summary`, `author`)
- `plans_get_note` — read a specific note by note ID (params: `plan`, `note_id`)
- `plans_list_notes` — list all notes for a plan (params: `plan`)
- `plans_get_plan` — read plan metadata, body, and task/note IDs (params: `name`)
- `plans_update_plan` — update a plan's body and metadata; creates if missing (params: `name`, `content`)
