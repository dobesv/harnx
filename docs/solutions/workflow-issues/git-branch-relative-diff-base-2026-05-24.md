---
title: "Git branch-relative diff base: origin/HEAD not merge-base"
date: 2026-05-24
category: workflow-issues
problem_type: logic_error
component: pantheon-agents
root_cause: incorrect git diff base selection for squash and review operations
resolution_type: code_fix
severity: medium
tags:
  - git
  - squash
  - diff-base
  - agent-prompts
  - origin-head
plan_ref: fixup-git-instructions-issue-641
---

## Problem

Agent prompts used incorrect git patterns for branch-relative operations: `git merge-base` was recommended as squash base, `<default-branch>` placeholders required manual detection, and `HEAD~1` fallbacks only captured the most recent commit instead of full branch history.

## Symptoms

- Squash operations missed commits when default branch was merged into feature branch
- `git diff HEAD~1` showed only the latest commit, not full branch changes
- Hardcoded `origin/main`/`origin/master` failed in repos with different default branch names
- Agent instructions used confusing placeholder syntax like `<default-branch>`
- Terminology error: prompts described `git diff --name-only` output as "commits" instead of "files"

## Investigation Steps

Issue #641 identified multiple stale git patterns across agent prompts:
1. Searched for `origin/master`, `origin/main`, `HEAD~1`, `merge-base` in agent files
2. Traced why `git merge-base` fails as squash base in merge-workflow teams
3. Verified `origin/HEAD` behavior across clone types (full, shallow, fresh)
4. Confirmed dot-syntax asymmetry: `git diff origin/HEAD...` (three-dot for cumulative diff) vs `git log origin/HEAD..` (two-dot for branch commits)

Tested with `git symbolic-ref refs/remotes/origin/HEAD` in standard and shallow clones — shallow clones do NOT create this ref by default.

## Root Cause

**Squash base rule inversion**: `git merge-base HEAD origin/<default-branch>` returns the nearest common ancestor. When the default branch is merged INTO the feature branch (merge-workflow), the merge-base advances past that merge commit, causing squash to miss the merged-in commits. `origin/HEAD` always points to the tip of the remote's default branch, which is the correct boundary.

**Dot syntax**: Git's three-dot diff (`A...B`) shows changes from merge base to B — correct for cumulative branch diff. Two-dot log (`A..B`) shows commits reachable from B but not A — correct for branch commit list. These must NOT be unified.

**HEAD~1 fallback**: Only captures the most recent commit. Feature branches often have multiple commits; `HEAD~1` missed the full branch scope.

## Solution

### 1. Squash Base Rule

Replace `git merge-base` with `origin/HEAD`:

```bash
# BEFORE (incorrect)
git reset --soft $(git merge-base HEAD origin/<default-branch>)

# AFTER (correct)
git reset --soft origin/HEAD
```

Add prerequisite note for missing `origin/HEAD`:
```bash
git fetch origin                    # ensure freshness
git remote set-head origin -a       # set if missing (shallow clones)
```

### 2. Dot-Syntax Asymmetry (Intentional)

Keep these patterns distinct:
- `git diff origin/HEAD...` — three-dot for cumulative branch diff
- `git log origin/HEAD..` — two-dot for branch commits list

Do NOT unify these to the same syntax.

### 3. Replace HEAD~1 Fallbacks

```bash
# BEFORE (misses full branch history)
git diff HEAD~1 --name-only

# AFTER (full branch scope)
git diff origin/HEAD... --name-only
```

### 4. Eliminate Default Branch Detection Logic

Replace `origin/main`, `origin/master`, `<default-branch>` with `origin/HEAD`. No detection logic needed — git resolves automatically via remote HEAD.

## Why This Works

`origin/HEAD` is a symbolic ref that always points to the remote's default branch tip. Unlike `<default-branch>` placeholders, it:
- Requires no branch-name detection
- Works regardless of whether the repo uses `main`, `master`, or custom names
- Self-resolves via git remote configuration

The prerequisite is that `origin/HEAD` exists. Full clones create it automatically. Shallow clones and some CI environments may not, requiring the recovery commands above.

## Prevention Strategies

**Verification checklist for git instruction changes:**
- [ ] All branch-relative diffs use `origin/HEAD...` (three-dot)
- [ ] All branch commit logs use `origin/HEAD..` (two-dot)
- [ ] No `HEAD~1`, `HEAD~N` as review-scope fallbacks
- [ ] No `<default-branch>` placeholders
- [ ] No `git merge-base` as squash base
- [ ] Prerequisite note for `origin/HEAD` setup included

**Smoke-test before merging:**
```bash
git rev-parse --verify origin/HEAD   # confirm ref exists
git diff origin/HEAD... --name-only  # confirm syntax works
git log --oneline origin/HEAD..     # confirm log works
```

**Terminology accuracy:**
- `git diff --name-only` → lists changed files, NOT commits
- `git log` → lists commits

## Related Issues

- GitHub Issue: #641 — Fix stale git patterns in agent prompts
- Files changed: `packages/pantheon/agents/shared/clio.md`, `shared/pytheas.md`, `shared/mnemosyne.md`, `shared/aristarchus.md`, `shared/argus.md`, `shared/atlas.md`, `shared/quality-review.md`, `packages/pantheon/agents/clio.md`, `packages/pantheon/agents/mnemosyne.md`
