---
harnx: minor
---

feat(access): optional startup user alias configuration (#2317)

Add optional user alias configuration (`users.yaml`) to expand authenticated caller identities for access checks and session visibility in `harnx-serve` and `harnx-a2a-server`:
- Load aliases once at startup from the default configuration directory (`<config_dir>/users.yaml`). When absent, alias expansion is skipped and callers retain their single authenticated identity. Present but malformed files (invalid YAML, empty, whitespace-only, comment-only, null documents, or schema violations) fail startup immediately with path context.
- Configuration is a top-level YAML sequence of entries with required `name` and `identities` fields; unknown fields are rejected. The `name` field is display metadata, never treated as an authenticated principal. An empty sequence or empty `identities` list is a valid no-op.
- Caller identity lookup scans entries in order and returns identities from the first matching entry. Matching is exact and case-sensitive. Overlapping entries do not merge (no transitive union), duplicate identities in an entry are silently accepted, and unmatched callers retain their singleton identity.
- Expanded identities apply only to caller authorization checks (agent visibility and session access under access rules). Original caller identity is preserved: new sessions in `harnx-serve` record the incoming raw user ID in session metadata, and new contexts in `harnx-a2a-server` record the raw user ID in the A2A binding. Stored session owners are never expanded.
- In `harnx-serve`, requests remain unrestricted when access rules (`access.yaml`) are absent; aliases alone never restrict access. In `harnx-a2a-server` without access rules, owner isolation checks accept stored owners contained in the caller's expanded identities, while preserving anonymous access and rejecting mixed anonymous/authenticated access.
- Request-local group and role memberships remain unexpanded and distinct from user identities.
- Alias configuration affects authorization boundaries and should be edited only by trusted operators.
