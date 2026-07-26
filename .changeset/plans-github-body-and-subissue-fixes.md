---
"harnx": minor
---

feat(plans): add `content` param for plan body, reject unknown params, and support `parent_issue` for sub-issue nesting

**Plan body via `content` param**
- Plans MCP tools now accept a `content` parameter to set the plan body directly
- Previously only `replace_content`/`append_content`/`replace_in_content` existed for body edits
- A stray `content` param was silently dropped, creating empty plan bodies

**Reject unknown parameters**
- Plan tool params now use `deny_unknown_fields` — unknown params are rejected with an error
- Prevents silent data loss from typos or misnamed parameters

**Sub-issue nesting via `parent_issue`**
- New create-time-only `parent_issue` parameter allows creating plans as GitHub sub-issues of an originating issue
- Enables hierarchical task organization in GitHub Issues
