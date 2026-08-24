---
harnx: patch
---
Keep web and TUI clients synchronized on canonical session IDs, live session activity, and complete reloaded assistant replies. Prevent hydration from duplicating externally submitted prompts, keep title maintenance inside its session lease, distinguish message roles, report unavailable session discovery, and recover damaged tool transcripts with visible warnings.

Keep the web installer lockfile and Assistant UI dependency family aligned so frozen installs and production builds succeed.
