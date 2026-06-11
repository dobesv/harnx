---
title: "find -exec Style Hook CLI Syntax for MCP Hooks Proxy"
date: 2026-06-10
category: integration-issues
problem_type: integration_issue
component: harnx-mcp-hooks-proxy
root_cause: "Shell-quoting complexity in hook command arguments motivated find -exec style argv token syntax"
resolution_type: code_fix
severity: medium
tags:
  - cli-parsing
  - hooks
  - argument-handling
  - shell-semantics
  - code-reuse
plan_ref: hooks-proxy-exec-syntax-673
---

## Problem

Users had to shell-quote hook commands within a single string argument, e.g. `--hook "echo 'hello world'"`. This is error-prone and differs from how `find -exec` handles commands cleanly as separate argv tokens. GitHub issue #673 requested `find -exec` style syntax: `--hook <TYPE> <CMD> [ARGS...] ;` with each token as a separate argv element, eliminating inner shell-quoting.

## Symptoms

- Users struggled with nested quoting: `--hook "sh -c 'echo $VAR'"`
- Inconsistent UX across workspace crates for hook specification
- No unified pattern for passing multi-token commands to hooks

## Investigation Steps

1. Reviewed existing `harnx-sandbox-run` crate — already implements `find -exec` syntax
2. Located reference implementation: `crates/harnx-sandbox-run/src/cli.rs` (`pre_parse_hooks` + `collect_hook_tokens`)
3. Found `build_hook_command` + `shell_quote` in `crates/harnx-sandbox-run/src/hooks.rs`
4. Verified pattern: pre-parse argv collecting tokens until `;` or `\;` terminator **before** clap sees args
5. Noted workspace-wide duplication: same `shell_quote` and `build_hook_command` exist in harnx-mcp-hooks-proxy, harnx-sandbox-run, harnx-hooks — candidate for extraction into harnx-core

## Root Cause

Traditional single-string hook args require users to mentally model shell quoting rules. The `find -exec` pattern solves this by treating each argv token as a literal until the terminator, then shell-quoting programmatically at dispatch time.

## Solution

Mirror the proven pattern from `harnx-sandbox-run`:

**Pre-parse loop** (`collect_hook_tokens`):

```rust
fn collect_hook_tokens<I>(iter: &mut I, flag: &str) -> Result<Vec<String>>
where
    I: Iterator<Item = String>,
{
    let mut tokens = Vec::new();
    for token in iter.by_ref() {
        if token == ";" || token == "\\;" {
            return Ok(tokens);
        }
        tokens.push(token);
    }
    bail!("unterminated {flag} (missing ';')")
}
```

**Shell-quoting** (`shell_quote`):

```rust
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_hook_command(command: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_quote(command));
    for arg in args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}
```

**Option parsing guard** (key design detail):

```rust
while let Some(token) = iter.next() {
    match token.as_str() {
        "--async" if command.is_none() => { async_hook = Some(true); }
        "--matcher" if command.is_none() => { matcher = Some(iter.next().context("--matcher requires REGEX")?); }
        _ => {
            command = Some(token);  // First non-option is the command
            args.extend(iter);       // Rest are args
            break;
        }
    }
}
```

The `if command.is_none()` guard ensures `--async`/`--matcher` are only consumed BEFORE the first command token. Same flags appearing after the command are treated as literal args.

**CLI usage**:

```bash
harnx-mcp-hooks-proxy \
  --pre-tool-use claude-command --async --matcher "bash_exec" /usr/bin/env-logger arg1 arg2 ';' \
  --post-tool-use claude-command /usr/bin/cleanup ';' \
  -- child-command --with --args
```

## Why This Works

1. **Pre-parsing before clap**: Raw argv is walked once, collecting hook tokens until `;`. Remaining args go to clap. This avoids clap trying to parse `--async` within hook definitions as its own flags.
2. **Per-token shell-quoting**: Each token is quoted individually and joined into a single `HookConfig.command` string, executed via `sh -c` downstream. Users don't need to quote.
3. **Option guard**: `--async`/`--matcher` only recognized before the command token — prevents accidental interception if a wrapped command uses the same flag names.
4. **Reused proven pattern**: harnx-sandbox-run already validated this approach — mirroring reduces risk and maintains workspace consistency.

## Prevention Strategies

**Code Review Checklist**:
- [ ] Does new hook CLI syntax mirror harnx-sandbox-run's `pre_parse_hooks` + `collect_hook_tokens`?
- [ ] Are options guarded with `if command.is_none()`?
- [ ] Is `shell_quote` used for each token (not the whole string)?

**Future Consolidation**:
- Extract `shell_quote` and `build_hook_command` into `harnx-core` to eliminate duplication across harnx-mcp-hooks-proxy, harnx-sandbox-run, harnx-hooks.

**Test Cases**:
- Hook with `--async` before command → recognized
- Hook with `--async` after command → treated as literal arg
- Hook with spaces in args → no quoting issues
- Unterminated hook definition → clear error message

## Related Issues

- **GitHub:** [#673](https://github.com/example/harnx/issues/673) — find -exec style hook args
- **Reference Implementation:** `crates/harnx-sandbox-run/src/cli.rs` — `pre_parse_hooks` + `collect_hook_tokens`
- **Related Solution:** [cli-hook-pre-parse-separator-2026-05-28.md](./cli-hook-pre-parse-separator-2026-05-28.md) — `--` separator boundary handling
