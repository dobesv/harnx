---
title: "PATH Shim Directory Pattern for Transparent Sandbox Wrappers"
date: 2026-06-02
category: integration-issues
problem_type: integration_issue
component: harnx-sandbox-run
root_cause: "Shim directory on PATH causes host-side recursion when sandbox passes PATH through and child resolves command[0] against that PATH"
resolution_type: workflow_improvement
severity: high
tags:
  - sandboxing
  - birdcage
  - path-manipulation
  - recursion-prevention
  - wrapper-scripts
  - self-name-dispatch
plan_ref: sandbox-shim-dir-recipe
---

## Problem

When using a PATH-prepended "shim directory" to transparently sandbox CLI tools, `harnx-sandbox-run` passes `PATH` through to the sandboxed child (baseline env passthrough). Birdcage resolves the command (`command[0]`) against this inherited `PATH`. If the shim directory is still first on `PATH` inside the sandbox, `harnx-sandbox-run -- node` re-resolves to the shim script → infinite recursion on the host.

## Symptoms

```
# Scenario: shim dir at /home/user/.local/share/harnx/sandbox-bin
# User runs: node --version
# Shim execs: harnx-sandbox-run -- node --version
# Inside sandbox, PATH still starts with shim dir
# Birdcage resolves "node" → shim script again → recursion

# Eventually hits:
bash: fork retry: Resource temporarily unavailable  # (process table exhausted)
# OR (with defense-in-depth whitelist):
Error: Permission denied (exec from non-whitelisted path)
```

## Investigation Steps

1. Reviewed `harnx-sandbox-run` source (`crates/harnx-sandbox-run/src/sandbox.rs:246`) — confirmed `PATH` is in `DEFAULT_ENV_PASSTHROUGH` baseline env list.

2. Traced sandbox behavior: birdcage inherits environment from parent process, including `PATH`. When child command is a bare name (not absolute path), birdcage resolves it via `PATH` lookup.

3. Identified that the shim directory, being first on `PATH`, would be found first when resolving commands like `node`, `claude`, `gemini`.

4. Recognized two defenses needed:
   - Shim MUST strip its own directory from `PATH` before exec-ing `harnx-sandbox-run`
   - Shim directory should NOT be on sandbox's exec whitelist (defense in depth)

5. Tested PATH-strip pattern with exact element matching to ensure substring-similar paths (e.g., `/path/to/shim-extra`) are preserved.

## Root Cause

**Recursion mechanism**: Shim dir prepended to `PATH` → shim execs `harnx-sandbox-run -- tool` → sandbox passes `PATH` through → child process inherits `PATH` with shim dir still first → birdcage resolves bare command name against `PATH` → finds shim again → infinite loop.

The recursion occurs on the HOST side (not inside the sandbox) because `harnx-sandbox-run` itself runs outside the sandbox and execs the shim again.

## Solution

### PATH-Strip Pattern

Each shim strips its own directory from `PATH` before exec-ing `harnx-sandbox-run`:

```bash
# Resolve own directory (canonical path)
self_dir="$(cd "$(dirname "$0")" && pwd -P)"

# Strip self_dir from PATH by exact element match (not substring!)
PATH="$({
  old_ifs=$IFS
  IFS=:
  for path_entry in $PATH; do
    if [ "$path_entry" != "$self_dir" ]; then
      if [ -n "${new_path-}" ]; then
        new_path="${new_path}:$path_entry"
      else
        new_path="$path_entry"
      fi
    fi
  done
  IFS=$old_ifs
  printf '%s' "${new_path-}"
})"
export PATH
```

**Key details:**
- Uses `IFS=:` to split on path separator
- Exact element comparison (`[ "$path_entry" != "$self_dir" ]`) — not substring match
- Rebuilds `PATH` preserving order of remaining elements
- Handles empty `PATH` gracefully (`${new_path-}` syntax)

### Self-Name Dispatch Pattern

Single script handles multiple tools via `basename "$0"`:

```bash
tool="$(basename "$0")"
# ... PATH strip ...
exec harnx-sandbox-run -- "$tool" "$@"
```

Create symlinks for tool variants:
```bash
chmod +x "$SHIM_DIR/node"
for t in yarn npm npx pnpm; do ln -sf node "$SHIM_DIR/$t"; done
```

This works when all symlinked tools have identical sandbox access rules (node-family: same caches, same project roots).

### Defense-in-Depth: Exec Whitelist

Document that shim directory is NOT on the sandbox's execution whitelist. Even if PATH-strip fails (e.g., symlinked PATH entry), birdcage blocks exec from shim dir inside sandbox, causing a clear "Permission denied" rather than silent recursion.

## Why This Works

1. **Exact-element strip prevents substring clobber**: `/path/to/shim-extra` is preserved when stripping `/path/to/shim`

2. **Self-name dispatch reduces boilerplate**: One script, N symlinks, identical access profiles

3. **Defense-in-depth fails safe**: Whitelist block produces visible error, not infinite loop

4. **Canonical path resolution (`pwd -P`)**: Handles symlinks to shim dir at the script level (though see edge cases below)

5. **Works with pseudo-vars**: `$GIT_ROOT`, `$NODE_PROJECT_ROOT`, `$GIT_COMMON_DIR` passed through literally (single-quoted) so `harnx-sandbox-run` resolves them inside sandbox context

## Known Edge Cases

1. **Symlinked PATH entry vs canonical `self_dir`**: If user's shell profile uses a symlinked path (e.g., `/links/me/.local/...`) while `pwd -P` resolves to canonical (e.g., `/nfs/users/me/.local/...`), exact match fails. Shim dir remains on `PATH`.

   **Mitigation**: Document that users should use canonical paths in shell profile. Defense-in-depth whitelist still prevents recursion (fails with exec denial).

2. **Trailing slash on PATH entry**: `/path/to/shim/` != `/path/to/shim`. Exact match fails.

   **Mitigation**: Document: avoid trailing slashes in PATH exports. Shell profile templates in docs don't add them.

3. **Empty PATH after strip**: If `PATH` contained only shim dir, result is empty string. Child process cannot resolve any commands — fails fast at exec.

Both edge cases are narrow and result in clear failure, not silent security bypass.

## Prevention Strategies

**When to Use This Pattern:**
- Transparent sandboxing of existing CLI tools
- Multiple tools share identical sandbox access rules
- Users want zero-config project access (pseudo-vars)

**When to Create Separate Shims:**
- Tools have different access profiles (e.g., `claude` needs `~/.claude` + hooks; `gemini` needs `~/.gemini`)
- Per-tool hook configuration differs

**Code Review Checklist:**
- [ ] PATH strip uses exact element match (not substring)?
- [ ] `IFS=:` used to split correctly?
- [ ] `pwd -P` used for `self_dir` resolution?
- [ ] Shim directory excluded from exec whitelist?
- [ ] Pseudo-vars single-quoted to prevent host expansion?
- [ ] `shellcheck disable=SC2016` for pseudo-var patterns?

**Testing:**
- Verify PATH strip removes shim dir while preserving similar paths
- Run `which -a tool` to confirm shim appears before real tool
- Test symlinked PATH entry scenario (should fail with exec denial, not recurse)

## Related Issues

- **GitHub Issue:** [#575 — Ability to use sandbox tooling as CLI wrappers](https://github.com/dobesv/harnx/issues/575)
- **Plan:** sandbox-shim-dir-recipe
- **Commits:** 25933701ee (shim-dir recipe), 8df2053917 (key points clarification)
- **Related Solution:** [birdcage-env-passthrough-2026-05-28.md](birdcage-env-passthrough-2026-05-28.md) — Baseline env passthrough design
- **Related Solution:** [sandbox-project-root-pseudo-vars-2026-06-02.md](../workflow-issues/sandbox-project-root-pseudo-vars-2026-06-02.md) — Pseudo-var syntax
