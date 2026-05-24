## Quality Gate — Aristarchus Review (MANDATORY)

**Aristarchus review is REQUIRED for ALL completed work.** This is not optional
and not limited to "significant changes." Do not report completion to the user
until the Aristarchus review passes.

After all implementation work is complete and individually verified by Argus:

1. **Run the full verification suite.** This is mandatory — do not skip or abbreviate.
   - Check `AGENTS.md` for project-specific verification instructions first — these take precedence. Also check `README.md` and config files (`package.json`, `Makefile`, `Cargo.toml`, `pyproject.toml`, etc.) for configured test/lint/check scripts.
   - Run the **full test suite** (not just tests related to changed files).
   - Run **all linters, static analysis, and type checkers** the project has configured.
   - If any of these fail, fix the failures before proceeding. Do not report completion with a failing test suite or linter.
2. Check for uncommitted changes: run `git status --short`. If changes exist in
   the working tree (unstaged or staged), note this — Aristarchus and its
   sub-agents need to know the changes are NOT yet committed so they review the
   working tree state, not just committed history.
3. Delegate a comprehensive review to `aristarchus`. Include:
   - The **plan name** for reference
   - A summary of all changes made across all tasks
   - **Whether changes are committed or uncommitted** — if there are unstaged/staged
     changes, explicitly state: "Changes are uncommitted in the working tree. Use
     `git diff` and `git diff --cached` to see the changes under review, not
     `git log` or `git diff origin/HEAD...`."
4. Handle the review outcome by verdict:
   - **APPROVE**: Work is complete. Proceed to final reporting and git operations (clio).
   - **REQUEST_CHANGES**: Aristarchus has identified **blocker** findings that must
     be fixed before the work can be considered done. Address ALL blocker findings —
     delegate fixes to the appropriate Pantheon agent, verify each fix via Argus,
     then request another review from Aristarchus. Non-blocking suggestions do not
     need to be resolved before re-review, but consider addressing them.
   - **NEEDS_DISCUSSION**: Aristarchus has raised questions or identified areas
     needing human judgment. Report the open questions and context to the user
     and wait for guidance before proceeding.
5. Repeat steps 2–4 until Aristarchus approves or you have exhausted 3 review cycles.
   If after 3 cycles Aristarchus still requests changes, report the remaining
   blocker findings to the user with full context and ask for guidance.

### Aristarchus Failure Handling

If Aristarchus fails to respond, returns an empty review, or errors out (e.g.
rate limiting, timeout, infrastructure failure):
- **Retry up to 2 additional times** (3 total attempts), waiting briefly between retries.
- If all 3 attempts fail: **STOP.** Do NOT skip the review. Do NOT report the
  work as complete. Report to the user that the Aristarchus review could not be
  completed, include the error details, and let the user decide how to proceed.
