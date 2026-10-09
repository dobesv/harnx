---
harnx: minor
---

feat(access): group and role access rules for harnx-serve and harnx-a2a-server (#2369)

Extend trusted request identity and access control rules with group and role memberships:
- Configure trusted group and role headers via `serve_group_headers` / `serve_role_headers` in `config.yaml`, environment variables `HARNX_SERVE_GROUP_HEADERS` / `HARNX_SERVE_ROLE_HEADERS`, or repeatable `--group-header <NAME>` and `--role-header <NAME>` CLI flags in `harnx-serve`.
- Configure trusted group and role headers via repeatable `--group-header <NAME>` and `--role-header <NAME>` CLI flags in `harnx-a2a-server`.
- No membership headers are trusted by default; unconfigured client headers are ignored. Header names are validated at startup.
- All configured membership headers contribute values; repeated headers and comma-delimited tokens are split, trimmed, and empty tokens are dropped. Any header byte failing UTF-8 conversion fails closed immediately with HTTP 401 (JSON-RPC code `-32000`) without echoing header contents.
- Access rules support distinct `users`, `groups`, and `roles` selector categories. Each category defaults to an empty list; rules require at least one non-empty selector category. Users, groups, and roles form distinct namespaces that never match across categories.
- Rule matching uses OR semantics across selector categories: a rule matches when its agent glob matches and at least one user, group, or role selector matches. Caller scopes union across all matching rules.
- Group and role rules can be group-only, role-only, or mixed with users. Existing user-only access rules remain fully compatible.
- Memberships are request-local and evaluated per request; revoking or altering a membership takes effect on the next HTTP request and is never persisted in session metadata, task storage, or NATS properties.
- Groups and roles do not replace caller authentication: protected endpoints still require a trusted user identity. Memberships grant scopes but never satisfy session ownership, which remains tied solely to immutable user IDs. Admin scope granted by any matching rule allows managing sessions across all users.
