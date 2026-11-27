# Publish plan snapshots with PR deliveries

> **User revisions (2026-11-26):** Simplification decisions supersede the earlier caller-rendering, Atlas plan-discovery, and per-publication public/content approval dialogue contracts below. Clio renders conservatively; Atlas consumes supplied plans/context; clean snapshots publish automatically without consent prompts or override flags. Git transaction, destination, visibility, and exact-byte disclosure safeguards remain.

## Summary

Publish the current plan and notes as Markdown in a separate Git history at `refs/plans/<branch>`. Add an immutable GitHub link to the delivery commit. Re-delivery updates the same file while preserving earlier snapshots. Work without a plan gets a retrospective.

## Issue

https://github.com/dobesv/harnx/issues/2977

## Context

### Goals and Non-Goals

- **R1 — Content:** Render the plan and relevant notes at delivery, appending `Decisions and learnings`. Without a plan, write `Goal`, `Approach`, `Verification`, `Decisions`, and `Learnings` from available context, commits, and review discussion. Attribute decisions and their recorded reasons accurately; distinguish actual verification from assumptions.
- **R2 — History:** Store one Markdown file on `refs/plans/<delivery-branch>`. The first commit is orphaned; later snapshots descend from the previous published snapshot. Reuse the filename across updates and the parent's SHA when tree and message are identical. Retain the ref indefinitely.
- **R3 — Git safety:** Use plumbing with compare-and-swap local ref updates and fast-forward-only remote pushes. Preserve normal `HEAD`, index, and worktree. Snapshot operations run with hooks and signing disabled. Push the exact checked candidate, including under concurrent publication. Plan refs are never force-pushed.
- **R4 — Disclosure safety (revised):** Before writing Git objects, validate the source path and file type, scan the exact publication bytes for credentials and personal data, and establish repository visibility via anonymous probe plus authenticated confirmation for private 404s. Inconclusive visibility or flagged bytes block publication before object/ref writes; Clio sanitizes and retries cleanly without consent override flags or approval prompts.
- **R5 — Delivery link:** After successful publication, add `Plan: <immutable blob URL>` as the delivery commit's final paragraph. Retain `Plan-Id: cid:plan:...` when a live plan exists. Return the published URL in delivery metadata.
- **R6 — PR workflow:** Keep manual PR creation and the existing stability wait. During normal PR follow-up, tidy a bare description or missing/stale `Plan:` link opportunistically when PR metadata matches the delivery, preserving user text.

## Tasks

- [x] `t1` — Implement and test snapshot publication (`packages/pantheon/scripts/publish-plan.sh`, `packages/pantheon/tool_servers/bash.yaml`, `crates/harnx-bash-tools/tests/it/publish_plan.rs`, `crates/harnx-runtime/tests/it/middleware/publish_plan.rs`)
- [x] `t2` — Integrate publication into delivery and resume (`packages/pantheon/agents/shared/*.md`, `crates/harnx-runtime/tests/it/agent_prompt_rendering.rs`, `.changeset/pantheon-plan-publishing.md`)
- [x] `feedback-docs` — Remove unwanted anonymous-request documentation note from `crates/harnx-proxy-auth/README.md`
- [x] `feedback-atlas-handoff` — Atlas consumes supplied plans/context without PR/commit discovery (retaining Sisyphus discovery)
- [x] `feedback-clio-rendering` — Centralize conservative snapshot rendering ownership in Clio
- [x] `feedback-auto-publish` — Automatic clean snapshot publication without approval prompts or override flags

## Decisions and learnings

### Architecture and workflow revisions

- **Packaged helper over proxy-enabled Bash:** Research confirmed `bash_wait_for_pr_stable` is a native `harnx-bash-tools` command template rather than a worker script tool; keeping `publish-plan.sh` on the proxy-enabled `bash_exec`/`bash_spawn` route preserves existing GitHub proxy credential injection without converting invocation routes.
- **Clio rendering ownership:** Callers (Atlas and Sisyphus) complete Mnemosyne reconciliation first, then pass the plan reference and compact execution context to Clio rather than duplicating rendering logic.
- **Handoff-only Atlas resume:** Atlas uses caller-supplied live plans, attached snapshots, or explicit context and does not run `pytheas` or search PR/commit history for plans; Sisyphus retains full plan-reference recovery (live `Plan-Id:`, legacy `Plan: cid:plan:...`, and authenticated HTTPS snapshot reads).
- **Automatic clean publication:** Per user simplification feedback, standing authorization covers conservatively rendered clean snapshots for both public and private GitHub repositories. Retired `--approve-public`, `--approve-content`, and `approval_required` flows are rejected as unknown flags.
- **Delivery bootstrap note:** On this PR's own delivery, the active host worker predates the new `packages/pantheon/tool_servers/bash.yaml` (`HARNX_PACKAGE_DIR` unset and installed script path inaccessible in the sandbox). After Clio reported the blocker, Atlas authorized invoking the checkout's `packages/pantheon/scripts/publish-plan.sh` directly via the same proxy-enabled Bash route with all script safeguards intact.

### Implementation safeguards

- **Pre-object disclosure and visibility gate:** `publish-plan.sh` validates origin fetch and push URLs to one matching credential-free GitHub HTTPS repository, opens the source inside the worktree or `/tmp` or `/var/tmp` via descriptor traversal (`O_NOFOLLOW`, `O_NONBLOCK`, regular singly linked owned file, 1 byte to 5 MiB UTF-8 without NUL), scans the captured bytes in memory, probes `api.github.com` anonymously (`curl -q --proxy "" --noproxy "*"`), and confirms 404s through the authenticated proxy before writing Git objects or refs.
- **Git transaction isolation:** Local ref reads/updates use `update-ref --no-deref` CAS with `core.hooksPath=/dev/null` and signing disabled. Network transport runs in a temporary bare repository sharing only `GIT_OBJECT_DIRECTORY`, keeping `FETCH_HEAD`, ambient push config, and credential helpers isolated. Contention retries up to 3 times by fetching the fast-forward winner and rebuilding the checked candidate; local-only conflicting history is left intact and refused.

### Verification and review provenance

- **Pre-rebase full presubmit (`1047a3eb5`, prior to `origin/main` `38f41df11`):** Atlas reported PASS for `cargo fmt --all -- --check`, `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask affected` + nextest (`6345 passed, 7 skipped, 0 retries`), `cargo xtask quality --base origin/main` (`0` added/worsened), `python3 scripts/check_changesets.py`, and `git diff --check`.
- **Pre-rebase focused & stress checks:** 20/20 stress iterations passed on 6 changed rendering tests and 6 helper tests; 39/39 focused helper, rendering, and proxy-fixture tests passed. Tests ran under `HARNX_TEST_SANDBOX=light` with disposable Git repositories and synthetic GitHub HTTP responses.
- **Reviews and knowledge reconciliation:** Aristarchus cycle 2 approved the original implementation (`32f83fc97`), and the simplification delta review (`ae4fe6294..1047a3eb5`) approved with no blockers. Mnemosyne evaluated `ae4fe6294..1047a3eb5` and confirmed no additional doc edits were needed.
- **Post-rebase verification (after rebasing onto `origin/main` `38f41df11`):** `cargo build --workspace`, 28 `harnx-runtime` tests (`config_check::shipped`, `agent_prompt_rendering`, `middleware_publish_plan_installed_via_proxy`), and 13 `harnx-bash-tools` `publish_plan` tests passed under `HARNX_TEST_SANDBOX=light`.
- **Unverified runtime boundaries:** Nested OS bubblewrap (`bwrap`) sandboxing (`--allow-read` on the installed script path) and live model prompt adherence remain unverified in automated tests.
