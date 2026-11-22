---
harnx: patch
pantheon: patch
---
Claude Fable 5.2 answers HTTP 400 to a request that identifies itself as a Claude Code release older than 2.1.290. If your own `claude` client config sends Claude Code's `user-agent` and `x-anthropic-billing-header`, raise the version in both before selecting `claude-fable-5-2`; clients that use `CLAUDE_API_KEY`, including the ones the packages ship, are unaffected.
