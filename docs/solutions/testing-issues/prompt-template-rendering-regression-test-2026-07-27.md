---
title: "Regression test for prompt-template rendering across shipped agents"
date: 2026-07-27
category: "testing-issues"
problem_type: test_failure
component: "harnx-runtime, pantheon-agents"
root_cause: "no CI verification that markdown agent prompts render without undefined variable errors"
resolution_type: test_fix
severity: medium
tags:
  - rendering
  - prompt-templates
  - minijinja
  - regression-test
  - agent-config
  - shared-variables
plan_ref: "natural-writing-prompts"
---

## Problem

Agent prompts under `packages/*/agents/` use MiniJinja templates with `{{variable}}` placeholders backed by `variables:` frontmatter entries. No CI test verified that all shipped agent markdown files render successfully. A missing `variables:` entry or typo in a placeholder name caused load-time failures discovered only at runtime.

## Symptoms

```
- MiniJinja `UndefinedBehavior::Strict` panics when rendering agent system prompt with undefined variable
- Missing `path:` file for a variable causes load failure
- Typos in `{{placeholder}}` names not caught until agent instantiated in a session
- Changes to shared prompt fragments could break multiple agents silently
```

## Investigation Steps

1. Reviewed `AgentConfig::from_markdown` — parses frontmatter `variables:` into `AgentVariables`
2. Traced `system_text()` — renders template with MiniJinja `UndefinedBehavior::Strict`, fails fast on undefined
3. Identified two agent sources: `packages/pantheon/agents/` (uses `shared/` fragments) and `packages/coding/agents/` (inlines)
4. Noted repo's Test Coverage Policy treats prompt markdown as executable behavior requiring verification
5. Designed test to iterate all `.md` agents, parse, resolve `path:` files, set shared variables, and assert render succeeds

## Root Cause

**No render verification in CI.** Agents declare variables in frontmatter and reference them in body:

```yaml
---
variables:
  - name: natural_writing
    path: shared/natural-writing.md
---
```

Body:
```markdown
{{natural_writing}}
```

If `natural_writing` is misspelled, the `path:` file doesn't exist, or the placeholder name doesn't match, `system_text()` fails. Without a test, broken prompts reached main.

## Solution

Added `crates/harnx-runtime/tests/agent_prompt_rendering.rs`:

```rust
#[test]
fn agent_prompt_rendering_renders_all_shipped_agents() {
    let Some(workspace_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|ancestor| ancestor.join("packages").is_dir())
    else {
        // Published crates do not include workspace package assets.
        return;
    };

    let agent_dirs = [
        workspace_root.join("packages/pantheon/agents"),
        workspace_root.join("packages/coding/agents"),
    ];
    let mut failures = Vec::new();

    for agent_dir in agent_dirs {
        // Iterate all .md files in agent directory
        for agent_path in /* sorted .md entries */ {
            // Parse markdown to AgentConfig
            let agent = AgentConfig::from_markdown(stem, &content)?;
            
            // Resolve path:-backed variables from disk
            let mut variables = AgentVariables::new();
            for variable in agent.defined_variables() {
                if let Some(relative_path) = &variable.path {
                    let value = fs::read_to_string(agent_dir.join(relative_path))?;
                    variables.insert(variable.name.clone(), value);
                }
            }
            
            // Set shared variables and render
            agent.set_shared_variables(variables);
            agent.system_text()?; // fails if any {{var}} undefined
        }
    }
    
    assert!(failures.is_empty(), "failed to render: {}", failures.join("\n"));
}
```

Key behaviors:
- Iterates every top-level `.md` under `packages/pantheon/agents/` and `packages/coding/agents/`
- Parses via `AgentConfig::from_markdown`, resolves `path:` files from disk
- Sets resolved values as `shared_variables`
- Asserts `system_text()` renders Ok (collects all failures before asserting)
- Skips gracefully when `packages/` isn't present (published-crate builds)

## Why This Works

**Fail-fast at load time.** The test runs in CI and catches:
- Missing `variables:` entry for a `{{placeholder}}` used in body
- Missing `path:` file for a declared variable
- Typos in variable names (either in frontmatter or placeholder)

UndefinedBehavior::Strict means any unreferenced context variable is ignored (MiniJinja doesn't error), but any referenced `{{var}}` without a value fails immediately.

**Known blind spot:** A variable declared in `variables:` but never referenced in the body still passes. The test only catches missing definitions for referenced placeholders, not unused declarations. This is acceptable — unused declarations waste space but don't break rendering.

## Prevention Strategies

**Test coverage requirement:**
- Any new agent markdown under `packages/*/agents/` automatically covered
- Any new `shared/*.md` fragment wired via `variables:` automatically exercised
- CI fails fast on template wiring regressions

**Adding cross-cutting prompt guidance:**
1. Create `packages/pantheon/agents/shared/<name>.md` with the guidance
2. Add `variables:` entry to each agent needing it:
   ```yaml
   variables:
     - name: <name>
       path: shared/<name>.md
   ```
3. Add `{{<name>}}` placeholder in agent body at desired position
4. Test will verify render succeeds

**For `coding` package:** Prompts are inlined (no `shared/` directory). Cross-package guidance must be duplicated inline in `packages/coding/agents/coder.md`.

## Related Issues

- **Issue:** #1248 — Add natural-writing style guidance to sample prompts
- **Related Solution:** [workflow-issues/dual-repo-pantheon-prompt-architecture-2026-07-18.md](../workflow-issues/dual-repo-pantheon-prompt-architecture-2026-07-18.md) — MiniJinja template composition rules
- **Related Solution:** [logic-errors/minijinja-system-prompt-templating-2026-04-25.md](../logic-errors/minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja context construction
