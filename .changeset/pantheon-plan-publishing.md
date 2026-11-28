---
pantheon: minor
---

Clio now publishes public-safe Markdown plans to `refs/plans/<branch>` via `bash_publish_plan` when a stored plan is delivered, linking `Plan: <blob URL>` alongside `Plan-Id:` in the delivery commit. Before creating Git objects or updating refs, the publisher verifies the GitHub destination and repository visibility and scans the rendered content for secrets or email addresses; failed or refused publication leaves out the `Plan:` link. Manual PR creation and the stability wait are unchanged.
