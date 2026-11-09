---
pantheon: patch
coding: patch
---

Make Pantheon's code review cheaper and faster. Muses and Judges now review by reading the code and quoting CI results, and report a missing test or a manual check rather than running tests, linters or builds themselves, unless reading can't settle a finding. Aristarchus keeps the previous verdict when a rebase leaves a PR's own diff unchanged, and reviews only the files that changed since the last round. Before review, Sisyphus and Atlas run the repository's pre-submit checks rather than the whole suite. They skip the review for rebases that leave the PR's commits unchanged, docs-only edits and comment replies. Minos now runs on Gemini 4 Argon. The shared shell guidance in both packages now asks for a `bash_exec` timeout well past a command's usual run time, and for terminating a spawned command that runs well past its usual time without new output.
