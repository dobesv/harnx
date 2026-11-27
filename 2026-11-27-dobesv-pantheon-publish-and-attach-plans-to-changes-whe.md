# Publish plan snapshots with PR deliveries

> **Scope revisions (2026-11-26 and 2026-11-27):** User revisions supersede the original caller-rendering, Atlas plan-discovery, per-publication consent dialogue, opportunistic PR-description cleanup, and Sisyphus PR-plan scraping/HTTPS-fallback additions. Clio alone owns conservative Markdown rendering; Atlas and Sisyphus pass minimal supplied plan/context handoffs; clean snapshots publish automatically without approval prompts or override flags; `sisyphus-resuming-prs.md` stays at the `origin/main` baseline. Git transaction, destination, visibility, and exact-byte disclosure safeguards remain.

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
- **R5 — Delivery link:** After successful publication, add `Plan: <immutable blob URL>` as the delivery commit's final paragraph. Retain `Plan-Id: cid:plan:...` when a live plan exists. Return the published URL and delivered `head_sha` in delivery metadata.
- **R6 — PR workflow (revised):** Keep manual PR creation and the existing stability wait unchanged. Opportunistic PR-description cleanup was removed from scope.

## Tasks

- [x] `t1` — Implement and test snapshot publication (`packages/pantheon/scripts/publish-plan.sh`, `packages/pantheon/tool_servers/bash.yaml`, `crates/harnx-bash-tools/tests/it/publish_plan.rs`, `crates/harnx-runtime/tests/it/middleware/publish_plan.rs`)
- [x] `t2` — Integrate publication into delivery and resume (`packages/pantheon/agents/shared/*.md`, `crates/harnx-runtime/tests/it/agent_prompt_rendering.rs`, `.changeset/pantheon-plan-publishing.md`)
- [x] `feedback-docs` — Remove unwanted anonymous-request documentation note from `crates/harnx-proxy-auth/README.md`
- [x] `feedback-atlas-handoff` — Atlas consumes supplied plans and context without PR/commit discovery
- [x] `feedback-clio-rendering` — Centralize conservative snapshot rendering ownership in Clio
- [x] `feedback-auto-publish` — Automatic clean snapshot publication without approval prompts or override flags
- [x] `feedback-minimal-delivery` — Remove PR-description cleanup, restore `sisyphus-resuming-prs.md` to `origin/main`, and minimize caller plan handoffs

## Decisions and learnings

### Architecture and scope decisions

- **Packaged helper over proxy-enabled Bash:** Research confirmed `bash_wait_for_pr_stable` is a native `harnx-bash-tools` command template rather than a worker script tool; keeping `publish-plan.sh` on the proxy-enabled `bash_exec`/`bash_spawn` route preserves existing GitHub proxy credential injection without converting invocation routes. Dedicated registered-tool conversion remains design discussion only.
- **Clio sole rendering and minimal caller handoffs:** Callers (Atlas and Sisyphus) complete Mnemosyne reconciliation first, then pass the plan reference (or supplied attachment/content) and compact execution context to Clio without duplicating rendering steps or helper mechanics.
- **Baseline Sisyphus resume and no PR-description editing:** Opportunistic PR-description cleanup was removed from Atlas, Clio, Sisyphus, `.changeset/pantheon-plan-publishing.md`, and prompt-rendering tests. `packages/pantheon/agents/shared/sisyphus-resuming-prs.md` was restored byte-identical to `origin/main` (dropping branch-added whole-message scraping and authenticated HTTPS snapshot fallback). Atlas uses supplied plans or context without searching PR or commit history.
- **Automatic clean publication:** Standing authorization covers conservatively rendered clean snapshots for both public and private GitHub repositories. Retired `--approve-public`, `--approve-content`, and `approval_required` flows are rejected as unknown flags.
- **Delivery bootstrap note:** Because the active host worker predates this branch's `packages/pantheon/tool_servers/bash.yaml` (`HARNX_PACKAGE_DIR` unset and installed script path absent in the sandbox), the caller authorized invoking the reviewed checkout script `packages/pantheon/scripts/publish-plan.sh` directly via the same proxy-enabled Bash route with all script safeguards intact.

### Implementation safeguards

- **Pre-object disclosure and visibility gate:** `publish-plan.sh` validates origin fetch and push URLs to one matching credential-free GitHub HTTPS repository, opens the source inside the worktree or `/tmp` or `/var/tmp` via descriptor traversal (`O_NOFOLLOW`, `O_NONBLOCK`, regular singly linked owned file, 1 byte to 5 MiB UTF-8 without NUL), scans the captured bytes in memory, probes `api.github.com` anonymously (`curl -q --proxy "" --noproxy "*"`), and confirms 404s through the authenticated proxy before writing Git objects or refs.
- **Git transaction isolation:** Local ref reads and updates use `update-ref --no-deref` CAS with `core.hooksPath=/dev/null` and signing disabled. Network transport runs in a temporary bare repository sharing only `GIT_OBJECT_DIRECTORY`, keeping `FETCH_HEAD`, ambient push config, and credential helpers isolated. Contention retries up to 3 times by fetching the fast-forward winner and rebuilding the checked candidate; local-only conflicting history is left intact and refused.

### Verification and review provenance

- **Earlier baseline approvals:** Aristarchus cycle 2 approved the initial implementation (`32f83fc97`) and the simplification delta review (`ae4fe6294..1047a3eb5`) approved with no blockers; those approvals stand for unchanged code.
- **Minimal-delivery presubmit (`dbb9c4e02..99437ea04`):** Atlas verified `cargo fmt --all -- --check`, `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask quality --base origin/main` (0 added/worse), `python3 scripts/check_changesets.py`, and `git diff --check` all PASS (`PROTOC=/usr/bin/protoc`, `HARNX_TEST_SANDBOX=light`).
- **Affected and stress tests (`dbb9c4e02..99437ea04`):** The first `cargo xtask affected` + nextest run had 6482 pass, 7 skip, and 1 failure on unmodified `nats_worker_sigterm_e2e::real_sigterm_hands_off_mid_llm_and_mid_tool_turns` (health endpoint timing); that test then passed 20/20 stress iterations with no code changes, and a second full affected nextest run passed 6483/6483 with 7 skipped and no retries. All 3 changed `agent_prompt_rendering` tests passed 20/20 stress iterations.
- **Scoped review and knowledge reconciliation:** Aristarchus reviewed the 5-file delta `dbb9c4e02..99437ea04` and issued APPROVE with no blockers (two optional test-hardening notes recorded, not applied). Mnemosyne reconciliation was intentionally skipped after evaluation because the delta is mechanical scope removal with no novel maintained knowledge.
- **Final rebase:** Rebased cleanly onto `origin/main` (`ec91ecd78`, `Cargo.lock`-only change) with no conflicts or source edits.
- **Unverified runtime boundaries:** Nested OS bubblewrap (`bwrap`) sandboxing (`--allow-read` on the installed script path) and live model prompt adherence remain unverified in automated tests.
