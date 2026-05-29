---
title: "Pre-Parsing Hook Definitions with -- Separator Boundary"
date: 2026-05-28
category: integration-issues
problem_type: integration_issue
component: harnx-sandbox-run
root_cause: "Hook pre-parser continued past -- separator, intercepting --hook flags intended for wrapped command"
resolution_type: code_fix
severity: high
tags:
  - cli-parsing
  - hooks
  - argument-handling
  - separator
  - shell-semantics
plan_ref: harnx-sandbox-run
---

## Problem

The `--hook TYPE CMD ARGS \;` syntax requires custom pre-parsing before clap. Without proper boundary handling, `--hook` tokens after `--` separator were intercepted as hook definitions, preventing wrapped commands from using literal `--hook` flags.

## Symptoms

- `harnx-sandbox-run -- my-tool --hook config` panics with "unterminated --hook"
- Wrapped tools cannot accept `--hook` as legitimate argument
- CLI parser violates standard `--` separator contract (everything after `--` is literal)

## Investigation Steps

1. Reviewed `pre_parse_hooks()` implementation — iterated all tokens without `--` check
2. Tested `harnx-sandbox-run -- /bin/echo --hook test` — parse error
3. Traced origin: hook collection loop consumed all tokens
4. Confirmed standard CLI convention: `--` terminates flag processing

## Root Cause

Pre-parse loop iterated over entire arg list:

**before** (broken):
```rust
while let Some(token) = tokens.next() {
    if token == "--hook" {
        // collect until ; or \;
        // PROBLEM: continues even after --
    }
    // ...
}
```

The `--` separator is standard CLI convention meaning "all following tokens are positional args, not flags." The hook parser violated this by continuing to intercept `--hook` after `--`.

## Solution

Break hook scanning at `--` and collect remaining tokens literally:

```rust
pub fn pre_parse_hooks(raw: Vec<String>) -> Result<(Vec<HookDef>, Vec<String>)> {
    let mut hooks = Vec::new();
    let mut remaining = Vec::new();
    let mut tokens = raw.into_iter().peekable();

    while let Some(token) = tokens.next() {
        if token == "--" {
            // Stop hook scanning; collect all remaining tokens as-is
            remaining.push(token);
            remaining.extend(tokens);  // drains iterator
            break;
        } else if token == "--hook" {
            // Collect hook tokens until ; or \;
            let mut hook_tokens = Vec::new();
            let mut terminated = false;

            for token in tokens.by_ref() {
                if token == ";" || token == "\\;" {
                    terminated = true;
                    break;
                }
                hook_tokens.push(token);
            }

            if !terminated {
                anyhow::bail!("unterminated --hook (missing ';')");
            }

            if hook_tokens.len() < 2 {
                anyhow::bail!("--hook requires TYPE and COMMAND");
            }

            hooks.push(HookDef {
                hook_type: hook_tokens[0].clone(),
                command: hook_tokens[1].clone(),
                args: hook_tokens[2..].to_vec(),
            });
        } else {
            remaining.push(token);
        }
    }

    Ok((hooks, remaining))
}
```

**Key insight**: `remaining.extend(tokens)` drains the iterator without further inspection, guaranteeing `--hook` after `--` reaches clap unchanged.

## Why This Works

1. **Standard contract honored**: Everything after `--` is positional
2. **No ambiguity**: Wrapped tool receives `--hook` as literal argument
3. **Termination before clap**: Hook extraction completes, then `remaining` goes to clap
4. **Composable**: Multiple `--` tokens work correctly (standard Unix behavior)

The pattern generalizes to any CLI with custom pre-parse requirements:
1. Pre-parse extracts special forms
2. Stop at `--`, collect rest verbatim
3. Pass remaining to standard parser

## Prevention Strategies

**Test Cases:**
- `--hook type cmd ; -- wrapped-tool --hook config` → hook extracted, `--hook config` passed through
- `-- --hook literal` → no hooks, `--hook literal` in remaining
- Multiple hooks before `--` → all extracted

**Best Practices:**
- Always respect `--` separator in custom CLI parsing
- `extend(tokens)` pattern is idiomatic for "collect rest verbatim"
- Document the `--` boundary behavior explicitly

**Code Review Checklist:**
- [ ] Does pre-parse stop at `--`?
- [ ] Are remaining tokens after `--` passed unchanged?
- [ ] Is there a regression test for `--hook` after `--`?

## Related Issues

- **GitHub Issue:** [#575 — Standalone CLI Sandbox Wrapper](https://github.com/dobesv/harnx/issues/575)
- **Plan:** harnx-sandbox-run
- **Related Solution:** [per-call-env-param-bash-mcp-2026-05-13.md](per-call-env-param-bash-mcp-2026-05-13.md) — `--env` before `--` separator pattern
