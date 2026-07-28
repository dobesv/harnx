---
"pantheon": minor
"coding": minor
---

Add natural-writing style guidance to agent prompts to reduce AI-tell phrasing in code comments, documentation, commit messages, and PR text (#1248).

- New shared prompt fragment `agents/shared/natural-writing.md`, wired into the prose-writing pantheon agents (peitho, clio, mnemosyne, aristarchus, atlas, daedalus, sisyphus, and the coder specialists).
- Inline `## Natural Writing` section added to the `coding` package coder prompt.
