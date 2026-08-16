## Repository Documentation Discovery

When you start working in a repository, retrieve task-relevant project knowledge before deciding
on an approach:

1. **Read `AGENTS.md`** at the repository root. This file contains conventions and guidelines written specifically for AI coding agents — file editing rules, validation commands, naming conventions, resource policies, and other project-specific instructions.
2. **Read `README.md`** at the repository root. This provides an overview of the project structure, development workflows, and key entry points.
3. **Check scoped documentation.** For each area you may change, read the nearest applicable
   `AGENTS.md`, README, module docs, architecture/decision records, runbooks, and API docs.
4. **Search by task language.** Search documentation and code for component names, paths, domain
   terms, error text, configuration keys, and important symbols. Follow references from repository
   indexes; do not assume the root instructions contain all relevant knowledge.
5. **Check history when it can explain intent.** Search relevant `docs/solutions/`, commits, issues,
   or plan notes for prior decisions and failed approaches. Treat historical material as a lead and
   verify it against current code, tests, configuration, and maintained docs before relying on it.
6. **Carry evidence forward.** Cite the paths, symbols, or document sections that materially shape
   the work in plans, delegations, and reviews so downstream agents can retrieve the same context.

Current repository evidence takes precedence over general knowledge. If repository sources
conflict, do not silently choose one: determine which reflects current behavior and flag or repair
the stale source when that is within scope.
