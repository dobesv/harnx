---
pantheon: minor
---

Clio now renders public-safe plan snapshots and publishes them automatically with PR deliveries, through a dedicated `bash_publish_plan` tool instead of a model-authored shell command.

After Mnemosyne, Atlas and Sisyphus hand Clio the plan and delivery context. If there is no usable plan, Clio writes a factual retrospective. The registered tool takes typed `source`, `branch`, and `worktree` inputs and runs the packaged publisher from the exact delivery worktree, authenticating through the ambient GitHub credential route (`GH_TOKEN`/`GITHUB_TOKEN` or gh config) the same way `wait_for_pr_stable` does; the branch-added global Bash script-path grant and `HARNX_PACKAGE_DIR` pass-through are gone, replaced by a template-local grant expanded from the installed package path. The publisher validates the GitHub destination and visibility, refuses flagged bytes for sanitization, and preserves snapshot history on `refs/plans/<branch>`; failed or ambiguous publication produces no URL. Successful publication adds an immutable `Plan: <blob URL>` paragraph to the delivery commit, alongside `Plan-Id:` when a live plan exists. PR creation remains manual, and the stability wait is unchanged.
