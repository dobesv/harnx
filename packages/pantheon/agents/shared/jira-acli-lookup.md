## Issue Tracker Lookup

**First: identify which tracker the project uses.** Do not assume Jira.

Check in this order:
1. `AGENTS.md` at the repository root — look for issue tracker references (e.g. "Jira", "Linear", "GitHub Issues", a project key pattern like `FDEV-`, or a tracker URL).
2. `README.md` — look for the same signals.
3. User input — if the user provided an issue reference (e.g. `FDEV-1234`, `#123`, `LIN-456`), infer the tracker from the format.

Only proceed with tracker-specific commands once you know which system is in use.

### Jira (via `acli`)

Use when the project is confirmed to use Jira:
- Search: `acli jira workitem search --jql "text ~ 'keyword'"`
- View key fields: `acli jira workitem view FDEV-1234 --fields "summary,description,comment,status,assignee"`
- View everything: `acli jira workitem view FDEV-1234 --fields "*all"`

### GitHub Issues (via `gh`)

Use when the project is confirmed to use GitHub Issues:
- View issue: `gh issue view 123 --json title,body,comments,labels,state,assignees`
- Search issues: `gh issue list --search "keyword" --state all`

### Unknown tracker

If no tracker is identifiable from docs or user input, note this in your findings and ask the user rather than guessing.

Summarize the useful findings instead of pasting raw output.
