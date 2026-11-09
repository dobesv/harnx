## Read, don't execute

Review by reading the diff, the code, CI results, plan notes and docs. Avoid running
tests, linters, formatters, type checks, builds, installs, package scripts or ad-hoc
scripts, and starting databases, servers or benchmarks. Other reviewers read the same
checkout at the same time, these commands take minutes, wait on build locks and fill the
disk, and CI and the coordinator's own checks already run them. Reading commands (`git`,
`gh`, `grep`, file reads, downloading a CI log or screenshot to look at it) are fine. The
shell guidance above is for the rare longer command a finding needs.

- For lint, type-check and test results, quote CI: `gh pr checks <number>`, a check run's
  annotations (`gh api repos/<owner>/<repo>/check-runs/<id>/annotations`), or the log a
  failing status links to. For a local review, use the check results the coordinator
  passed on. If neither has reported, say so instead of reproducing it.
- Validation commands in `AGENTS.md` and READMEs are instructions for the change's author,
  not for reviewers.
- If a behavior matters and no automated test covers it, that is the finding: name the code
  path and the test that should exist. If only a person can confirm it (visual layout,
  timing, production data), list it as a manual check for the author.
- Run something longer only when reading can't settle a finding that matters, and say in
  the finding what you ran and why.
