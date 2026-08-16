## Repository Context Research

Before planning or acting, build a task-specific context set. For every non-trivial task this is a
required research phase, whether planning is interactive or immediately followed by execution.
The goal is not exhaustive archaeology; it is to find current constraints, concurrent work, and
prior reasoning that could change the approach.

Delegate repository research to `pytheas` when available so findings can be cached and reused.
Give it the plan name, task statement, likely work areas, and the protocol below. Existing findings
may satisfy a step only when they include provenance, cover the same scope, and have been checked
against the current branch; otherwise refresh them.

1. **Extract search cues.** Identify affected paths and symbols, component and domain names, error
   text, configuration keys, issue references, and proposed techniques.
2. **Read current repository guidance.** Read the root repository map and the nearest applicable
   `AGENTS.md`/README files for likely work areas. Follow links to maintained architecture,
   decision, API, runbook, and troubleshooting docs.
3. **Inspect current implementation.** Search code, tests, configuration, and docs with several
   precise cues. Prefer sources closest to the behavior. Record the path and section/symbol for
   facts that shape the work.
4. **Search project records.** Identify the project's issue tracker, then search the referenced
   issue plus related, duplicate, blocking, or recently active issues using the same cues. Search
   open and recently merged pull requests that touch the component or discuss the issue. Read the
   relevant review threads and status; distinguish accepted decisions from proposals and abandoned
   work. If the tracker or PR data is unavailable, record that limitation and continue.
5. **Search version history.** Use path- and symbol-scoped history to recover rationale and prior
   regressions: start with `git log --oneline -- <path>`, then use `git log -S'<string>' -- <path>`
   or `git log -G'<regex>' -- <path>` for important behavior and `git blame` only for specific
   unexplained lines.
   Inspect relevant commits rather than relying on their subjects. Include linked issue/PR
   references when present.
6. **Check curated historical knowledge.** Search `docs/solutions/` and relevant plan notes for
   prior attempts, failure modes, and decisions.
7. **Triangulate.** Issues, PRs, review comments, commits, plan notes, and solution docs are evidence
   of historical intent—not proof of current behavior. Verify their current-state claims against
   the sources from steps 2–3. Identify contradictions and surface stale guidance as work rather
   than silently carrying both versions forward.

Keep searches bounded and relevance-driven. Broaden only when results expose an unresolved
dependency, contradiction, regression, or likely parallel effort. A trivial edit may need only the
applicable instructions and current file; do not use triviality to bypass research for behavior or
architecture changes.

Report only material results under `Repository Knowledge`:

- current constraints or patterns, with source paths/symbols
- relevant issues and pull requests, with identifiers, status, links, and the decision or risk
- relevant commits, with hashes, affected paths, and verified rationale
- prior decisions or failed approaches, marked as historical and verified or unverified
- conflicts, parallel work, or stale sources that should change the plan
- queries/sources checked and material access limitations
- `No relevant repository knowledge found` when the search is empty

Cache this synthesis in a `repository-knowledge` plan note when a plan exists. Do not dump search
results, treat absence as blocking, or present any historical source as proof of current behavior.
