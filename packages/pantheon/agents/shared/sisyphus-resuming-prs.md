When asked to continue work on an existing pull request, pick up where another
agent left off, or review a PR's implementation:
1. Delegate to `pytheas` to fetch the PR context (description, commits, changed files).
2. Look in Pytheas's response for a plan reference in commit trailers (Plan-Id: cid:plan:... or Plan: cid:plan:...).
3. If found, call `plans_get_plan` with the plan URL to load the original plan
   and accumulated context (learnings, decisions, problems).
4. Use the plan as your reference for understanding the original intent, what was
   already done, and what may remain. Check todo statuses with `plans_list_tasks` to see
   which tasks are open or closed. Use `filter: "all"` to include closed tasks.
   Results are `{items, next_cursor}` with a default limit of 20 (clamped to 1..100).
   Pass `next_cursor` as `cursor` until null; filtered pages can be empty before the end.
