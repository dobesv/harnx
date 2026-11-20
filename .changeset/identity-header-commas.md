---
harnx: major
---
Make header comma handling explicit for user identities and group/role memberships.

**Breaking:** `header:NAME` and bare `NAME` now treat each field as one opaque, trimmed value without splitting commas or unquoting. User identities retain the first repeated field; membership headers collect every repeated field. This preserves DNs such as `X-Amzn-Mtls-Clientcert-Subject: CN=alice,OU=eng,O=Acme Corp`, including RFC 2253 escaped commas.

Migrate group/role configurations that relied on comma-separated lists from `X-Groups` to `header-csv:X-Groups` (and similarly for roles). This applies to `serve_group_headers` / `serve_role_headers`, their environment variables, and both servers' `--group-header` / `--role-header` flags. To retain legacy first-comma-item user IDs, select `header-csv:NAME` in `--user-id-source` / `serve_user_id_sources` or A2A's `--user-id-header`.

`header-csv:` uses one shared list parser for identities and memberships. Double-quoted items can contain commas, so `"CN=admins,OU=eng,O=Acme", "CN=ops,O=Acme"` supplies two DN groups. Quoted `\"` and `\\` decode to quote and backslash; other escapes stay literal. Unquoted `\,` does not split and stays literal. Repeated membership fields are comma-joined before parsing. Malformed lists fail closed; user identities also reject an empty first item. Unsupported membership source kinds fail startup. Cookie, bearer, and forwarding-header behavior is unchanged.
