---
harnx: major
---
**Breaking change**: bare `compaction_agent` and `model`/client names in package agents now resolve within the same package by default.

## What changed

Packaged agents now use package-relative name resolution for `compaction_agent` references (#586) and `model`/client references (#585), matching the existing MCP/ACP server namespacing behaviour.

| Reference in a package agent | Resolves to |
|---|---|
| `foo` | `pkg/foo` (same package) |
| `other/foo` | `other/foo` (explicit cross-package, unchanged) |
| `/foo` | `foo` (top-level, leading slash stripped) |

- **`compaction_agent`**: Resolved at call time in `compact_session()`. A bare name is qualified with the active agent's package prefix before looking up the agent.
- **`model` / client name**: Resolved at load time in `apply_package_agent_transforms()`. The client-name part of a model string (e.g. `openai` in `openai:gpt-4o`) is qualified. Package clients themselves are also renamed from `openai` to `pkg/openai` when loaded from a package directory.

## Why

Prevents name collisions between packages and makes packages self-contained — a package can define its own client named `openai` without conflicting with a top-level client of the same name, and an agent inside the package will use that package-local client by default.

## Migration

Any package agent that previously relied on a bare name resolving to a top-level resource must be updated:

- Prefix with `/` to escape to the top-level namespace:
  ```yaml
  compaction_agent: /my-global-compactor
  model: /openai:gpt-4o
  ```
- Cross-package references use `other_pkg/name` syntax (unchanged):
  ```yaml
  compaction_agent: shared-tools/summarizer
  model: shared-tools/openai:gpt-4o
  ```

Top-level agents (not inside a package) are **not affected** — their bare name references still resolve globally.
