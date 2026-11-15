---
harnx: minor
---
Derive caller identity from Atlassian Forge Invocation Tokens and bearer JWT claims in `harnx-a2a-server`.

Configuration has two parts:
1. Bearer identity sources (`forge-fit`, `bearer-claim:CLAIM` via `--user-id-header`) read caller identity from the `Authorization: Bearer` token.
   - For `forge-fit`: user calls format as `CN=<principal>,OU=<context.cloudId>,O=atlassian.com`; automation and anonymous calls format as `OU=<context.cloudId>,O=atlassian.com`.
   - For `bearer-claim:CLAIM`: uses the named top-level string claim from the token.
2. Built-in JWKS verification (`--bearer-jwks-url`, `--bearer-issuer`, `--bearer-audience`) verifies RS256 token signatures against remote keys with 10-second clock-skew leeway, requires `exp`, checks `nbf`/`iat`, and caches keys with single-flight refetches throttled to one per 30 seconds on unknown `kid`.

Built-in verification is recommended. When verification flags are omitted, bearer sources operate in decode-only mode, safe only behind a reverse proxy that verifies tokens before forwarding requests. Note AWS ALB's `jwt-validation` action cannot verify Forge Invocation Tokens because it requires `iss` to be a valid URL and the FIT issuer is the string `forge/invocation-token`; use the server's built-in verification for Forge deployments. To require a valid token, configure the bearer source as the only `--user-id-header` source plus verification flags. Present bearer tokens that fail decoding or verification fail closed with HTTP 401 / JSON-RPC -32000 and do not fall through to later sources; requests lacking an `Authorization` header fall through to later sources.
