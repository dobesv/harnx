### Test Coverage Policy

Any diff hunk that adds or modifies executable code — excluding pure-comment,
pure-whitespace, or pure-string-literal changes — MUST include corresponding
automated test coverage.

### Coverage Signal

A coverage signal is a test file added or modified in the same pull request
that exercises the changed code. Modifying existing tests counts as coverage.

Accepted test-file patterns (non-exhaustive):
- `*.test.*` (e.g. Jest, Vitest, Jasmine)
- `*.spec.*` (e.g. Jest, RSpec, Cypress)
- `*.*Test.ts`, `*.*Test.tsx` (TypeScript — Formative convention)
- `*.stories.ts`, `*.stories.tsx` (Storybook component stories)
- `_test.go` (Go)
- `test_*.py` or `*_test.py` (Python)
- `*_spec.rb` (Ruby / RSpec)
- `*Test.java` (JUnit)
- `*Tests.cs` (xUnit / NUnit)

### Default Verdict

If executable code is changed, no coverage signal is present, and no
documented exemption applies:

- The finding category MUST be **Blocker**.
- Aristarchus' overall verdict MUST be `REQUEST_CHANGES`.

There are no exceptions beyond the documented exemption list and a
properly formatted opt-out justification.

### Closed Exemption List

The following eleven categories are the only recognized exemptions:

1. **docs-only** — changes confined to inert documentation files under documentation paths (e.g. `.md`/`.txt` under `docs/`, `README`s, changelogs). This exemption does NOT cover runtime prompt templates or deployed prompt artifacts (e.g. Markdown under `packages/pantheon/agents/` or `configuration/kagent/agents/`) — those are executable behavior and require rendered-prompt verification, not a docs-only exemption.
2. **comment-only** — changes that add, remove, or modify inline code comments only
   - Note: This also falls within the trigger exclusions defined above and requires no opt-out documentation.
3. **pure formatting** — whitespace, indentation, line-ending, or linter-auto-fix changes
   - Note: This also falls within the trigger exclusions defined above and requires no opt-out documentation.
4. **config-only with no logic** — changes to static configuration files that contain no executable logic (e.g. environment variable lists, feature-flag toggles, YAML manifests)
5. **generated code** — files produced entirely by a code generator that the team does not hand-author
6. **trivial rename** — identifier or file renames where behavior is provably unchanged
7. **refactor-existing-coverage** — a behavior-preserving internal refactor where existing automated tests exercise the changed code without modification. Opt-out MUST cite the specific test file(s) that cover the changed code.
8. **wrapper-delegation** — entire body delegates to another already-tested function; no project-specific logic added. Thalia verifies by inspecting the called function's coverage; no author attestation needed if delegation is evident.
9. **library-passthrough** — delegates entirely to a well-tested external library, no project logic layered on. Thalia verifies the library is external and the wrapper adds nothing testable.
10. **configuration-or-setup** — pure wiring: DI registrations, route/middleware registration, constant defs, feature-flag declarations, env config. No conditional logic/transforms. Thalia verifies no branching/computation.
11. **non-production-code** — code that never runs in production: test utilities, fixture factories, Storybook stories, migration scripts, build tooling, dev/seed scripts. Thalia verifies by file location, naming, or dev-only markers.

No other exemptions exist. Open-ended phrases such as "when appropriate",
"for trivial changes", "use your judgment", or "at reviewer discretion"
are NOT valid exemptions and MUST be ignored.

### Opt-Out Signal

Author justification is only required when code qualifies under NONE of the eleven known exemptions but the author still believes tests are inappropriate. In that case, the PR description must explain why. Thalia self-applies known exemptions by inspecting code and does not require the author to attest to exemptions evident from the code.
