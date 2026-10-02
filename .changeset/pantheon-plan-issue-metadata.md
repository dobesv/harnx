---
pantheon: patch
---

Update agent prompts to store issue references in plan metadata (`github_issue` paired with `github_owner_repo`, or `external_task_url`) instead of plan notes. Clio inspects structured metadata first when composing commit messages and preserves arbitrary task URLs or repo-qualified GitHub issue references. Retains legacy issue-note fallback and the `Issue: none` decline marker.
