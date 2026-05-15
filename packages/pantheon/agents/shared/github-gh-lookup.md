## GitHub lookup with `gh`

When you need GitHub issue or pull request context, use `gh` directly from the shell. Typical commands:
- View an issue: `gh issue view 123 --json title,body,comments,labels,state,assignees`
- Search issues: `gh issue list --search "keyword" --state all`
- View a pull request: `gh pr view 123 --json title,body,author,state,baseRefName,headRefName,reviews`
- List changed files: `gh pr diff 123 --name-only`
- View PR comments: `gh pr view 123 --comments`

Summarize the useful findings instead of pasting raw output.
