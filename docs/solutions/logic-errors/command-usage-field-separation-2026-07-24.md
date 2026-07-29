---
title: "Separate Command.name from usage hints — eliminate completion-to-literal-syntax bug"
date: 2026-07-24
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime, harnx-tui"
root_cause: "Single Command.name field overloaded as both dispatch/completion key and help display"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - completion
  - dot-commands
  - data-structure-design
  - separation-of-concerns
plan_ref: "decouple-command-usage-1168"
---
## Problem

`Command.name` in the `COMMANDS` table doubled as both the dispatch/completion key and the `.help` usage-hint display. Commands whose names embedded usage syntax (e.g. `.rewind <n>`, `.edit message <n>`, `.info mcp [server]`) tab-completed to the literal usage string instead of a real command.

## Symptoms

- Typing `.rew` and pressing TAB completed to `.rewind <n> ` instead of `.rewind ` — the `<n>` placeholder appeared in the input buffer
- `.edit message ` and `.delete message ` appeared twice in first-word completions due to name collision
- Subcommand completion arms for `.edit` and `.delete` were incomplete (missing `message`)
- Help output showed correct syntax, but completions polluted with placeholder tokens

## Investigation Steps

1. Traced `compute_completions` in `harnx-tui/src/input.rs` — first-word completion iterated `COMMANDS` and matched on `c.name.starts_with(filter)`
2. Found `COMMANDS` entries like `Command::new(".rewind <n>", "Rewind session...")` where the name literal included syntax
3. Inspected `dump_help` — already formatted `name + usage` concepts but relied on string parsing
4. Reviewed earlier PR #103 fix for `.title [generate|now]` — same bug class, patched ad-hoc in completion logic
5. Recognized systematic issue: one field serving two roles

## Root Cause

`Command` struct used a single `name` field for both:
1. **Dispatch key** — matching user input, routing to handlers
2. **Help display** — showing argument syntax in `.help` output

Overloading caused completion logic to offer literal `<n>`, `[server]`, `<n>-<m>` tokens as completions. Multiple commands sharing name prefix (`.edit message`, `.edit session`) produced duplicate entries because deduplication was on full name.

## Solution

Added dedicated `usage: Option<&'static str>` field to `Command` struct:

```rust
#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub usage: Option<&'static str>,
    pub description: &'static str,
}

impl Command {
    const fn new(name: &'static str, desc: &'static str) -> Self {
        Self { name, usage: None, description: desc }
    }

    const fn with_usage(name: &'static str, usage: &'static str, desc: &'static str) -> Self {
        Self { name, usage: Some(usage), description: desc }
    }
}
```

Names are now bare dispatch keys (`.rewind`, `.edit message`, `.info mcp`). `dump_help` recombines:

```rust
fn dump_help(output: &mut (dyn Write + Send)) -> Result<()> {
    let head = COMMANDS.iter().map(|cmd| {
        let label = match cmd.usage {
            Some(usage) => format!("{} {usage}", cmd.name),
            None => cmd.name.to_string(),
        };
        format!("{label:<24} {}", cmd.description)
    });
    // ...
}
```

First-word completion in `input.rs` deduplicates by bare name:

```rust
let mut seen = HashSet::new();
let commands: Vec<_> = COMMANDS
    .iter()
    .filter(|c| c.name.starts_with(filter) && seen.insert(c.name))
    .map(|c| (format!("{} ", c.name), Some(c.description.to_string())))
    .collect();
```

Added real subcommand completion arms in `completion_split.rs` for `.edit` and `.delete` (added `message`).

## Why This Works

Separating the machine key (`name`) from the human display (`usage`) eliminates the entire bug class structurally. Completion logic now operates on clean keys, help formatting combines for display. The `seen` HashSet deduplication handles cases where multiple usage variants share a base command (`.edit message`, `.edit session`).

This follows the earlier `.title` fix in PR #103 but solves the problem at the data-structure level instead of patching completion logic per-command.

## Prevention Strategies

**Test Cases:**
- `command_completions_separate_names_from_usage_and_offer_subcommands` in `harnx-tui/src/tests.rs` asserts:
  - Completions never contain `<n>`, `[server]`, `[name]`, `<n>-<m>`
  - Deduplication: `.edit message` and `.delete message` appear exactly once
  - Subcommand sets are exact (`.edit`, `.delete` arms)

**Code Review Checklist:**
- [ ] Do struct fields have single, clear purposes?
- [ ] Is display-formatted data derived from normalized keys?
- [ ] Are completion keys machine-friendly (no placeholders)?

**Best Practices:**
- Never overload one field as both machine key and human display
- When `.help` shows formatted data, derive it from normalized storage
- Regression tests for "completion never contains placeholder" patterns

## Related Issues

- **Issue:** [#1168](https://github.com/dobesv/harnx/issues/1168) — Command usage regression
- **Earlier Fix:** PR #103 — `.title` command similar bug, patched ad-hoc
- **Related Solution:** [logic-errors/dot-commands-picker-signals-2026-05-10.md](./dot-commands-picker-signals-2026-05-10.md) — Runtime-TUI command contract
