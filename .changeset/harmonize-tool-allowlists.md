---
harnx: major
coding: patch
pantheon: patch
---
Replace filesystem roots and per-tool extra path flags with shared explicit allow paths and opt-in batches. Existing tool-server YAML and sandbox-run invocations must migrate to the new flags and environment variables.
