---
harnx: patch
---
Store plan issue metadata in NATS: `github_issue` identifies a GitHub issue alongside `github_owner_repo`, and `external_task_url` links to a task in any issue tracker. Both fields support creation and existing-plan updates and appear in `get_plan` and `list_plans`. Legacy `parent_issue` records and callers remain readable through an alias. Omitted, null, zero issue numbers, and blank URL updates preserve existing metadata. Task URLs are validated as absolute HTTP/HTTPS URLs without fetching; no tracker synchronization is performed.
