## Daedalus Lookup Tools

Daedalus has scoped tools (not shell access) for reading GitHub and Jira:

### GitHub Issues

View an issue:
- `bash_gh_issue_view` — params: `number` (required), `repo` (required, format `owner/repo`)
- Returns JSON with: `title`, `body`, `comments`, `labels`, `state`, `assignees`

Search issues:
- `bash_gh_issue_list` — params: `query` (required), `repo` (optional), `state` (optional: `open`/`closed`/`all`), `limit` (optional, 1-100)
- Returns JSON array with: `number`, `title`, `state`, `labels`, `updatedAt`

### GitHub Pull Requests

View a PR:
- `bash_gh_pr_view` — params: `number` (required), `repo` (optional)
- Returns JSON with: `title`, `body`, `author`, `state`, `baseRefName`, `headRefName`, `reviews`, `url`

View PR comments:
- `bash_gh_pr_comments` — params: `number` (required), `repo` (optional)
- Returns human-readable discussion thread

List changed files:
- `bash_gh_pr_files` — params: `number` (required), `repo` (optional)
- Returns file paths, one per line

### Jira

View a work item:
- `bash_jira_view` — params: `key` (required, format `PROJECT-123`), `fields` (optional, defaults to `summary,description,status,assignee,comment`)
- Returns the selected fields as text

Search work items:
- `bash_jira_search` — params: `query` (required), `limit` (optional, 1-100)
- Returns JSON array

### Tracker Selection

Before using these tools, identify which tracker the project uses. Check:
1. `AGENTS.md` at the repository root
2. `README.md`
3. User input (issue key format hints at tracker)

Use GitHub tools for GitHub Issues, Jira tools for Jira. If unknown, ask the user.
