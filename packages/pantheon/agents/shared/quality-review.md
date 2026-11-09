## Quality Gate — Aristarchus Review (MANDATORY)

**Aristarchus review is REQUIRED for all completed work that changes code,** apart from
the exemptions below. This is not limited to "significant changes." Do not report
completion to the user until the Aristarchus review passes.

After all implementation work is complete and individually verified by Argus:

1. **Run the repository's pre-submit checks.** This is mandatory — do not skip it.
   - Use the checklist in `AGENTS.md` and the scoped docs it points to; these take
     precedence. Without one, check `README.md` and config files (`package.json`,
     `Makefile`, `Cargo.toml`, `pyproject.toml`, etc.) for configured scripts. Run the
     formatters, linters and type checks on the changed files or packages, and the tests
     of the packages you changed.
   - Unless that checklist asks for it, don't run the whole repository's test suite. CI
     runs it on the pull request, and in a shared checkout or sandbox it can take an hour
     and run out of disk and memory. If CI then fails, fix it as review feedback.
   - Wait for these checks to finish before delegating the review. Never leave a build,
     lint or test job running while Aristarchus and its reviewers read the same checkout.
   - If any check fails, fix it before proceeding.
2. Check for uncommitted changes: run `git status --short`. If changes exist in
   the working tree (unstaged or staged), note this — Aristarchus and its
   sub-agents need to know the changes are NOT yet committed so they review the
   working tree state, not just committed history.
3. Delegate a comprehensive review to `aristarchus`. Include:
   - The full **plan URL** (`cid:plan:...`) for reference
   - A summary of all changes made across all tasks
   - **Whether changes are committed or uncommitted** — if there are unstaged/staged
     changes, explicitly state: "Changes are uncommitted in the working tree. Use
     `git diff` and `git diff --cached` to see the changes under review, not
     `git log` or `git diff origin/HEAD...`."
   - The checks you ran in step 1 and their results, so reviewers don't run them again.
   - When your run has a deadline, `timeout_secs`: your remaining time minus 20 minutes
     for handling the verdict and delivery. If that leaves less than 30 minutes, don't
     start the review: record in the plan that the checks passed and the review is still
     to do, and report that instead of completion.
   - When Aristarchus already approved this work and you've changed only a few files
     since, ask it to review just those changes and name them.
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
5. Repeat steps 1–4, so the check results you pass on match the code under review, until
   Aristarchus approves or you have exhausted 3 review cycles.
   If after 3 cycles Aristarchus still requests changes, report the remaining
   blocker findings to the user with full context and ask for guidance.

### Changes that skip the review

Run step 1, but skip steps 2–5, when the turn adds no code of your own:
- a rebase or base-branch merge that leaves the pull request's own commits unchanged
  (`git range-diff` shows no change to them), including a lockfile regenerated to
  resolve a conflict;
- documentation-only or comment-only edits, except comments and Markdown that a compiler,
  tool or agent reads, such as `//go:build` lines or agent prompts;
- replies to review comments that change no files.

Say in your report, and in any pull request comment, that the review was skipped and
why. A conflict resolved by hand that changes behavior is new code: review it.

### Aristarchus Failure Handling

If Aristarchus fails to respond, returns an empty review, or errors out (e.g.
rate limiting, timeout, infrastructure failure):
- **Retry up to 2 additional times** (3 total attempts), waiting briefly between retries.
  When it ran out of the time you gave it, don't retry with less: report the review as
  still to do.
- If all 3 attempts fail: **STOP.** Do NOT skip the review. Do NOT report the
  work as complete. Report to the user that the Aristarchus review could not be
  completed, include the error details, and let the user decide how to proceed.
