# Tool Server Credentials

Users can connect tool servers — starting with remote MCP servers such as
Atlassian/Jira, GitHub, Pendo and Formative — to **their own** account, so
tools act with their identity and their access limits. Credentials are stored
encrypted in NATS, per user and per tool server, and users connect them
without ever pasting a secret into chat.

Only `harnx-mcp-remote` in native mode supports **per-user** stored credentials
today. (For a single machine identity in the stdio proxy, see
[Proxy mode: OAuth client credentials](#proxy-mode-oauth-client-credentials-machine-to-machine).)
The crate backing the storage (`harnx-credentials`) is reusable by other tool
servers, but no other server is wired up.

## How a connection works

Each credentialed tool server publishes three management tools alongside its
upstream tools:

| Tool | What it does |
| :--- | :--- |
| `<server>_connect` | Starts a connect flow and shows a link to open in a browser |
| `<server>_disconnect` | Removes your stored credential; your tools disappear from your sessions |
| `<server>_credential_status` | Shows whether you are connected, as whom, and when the credential expires |

The model can call `<server>_connect` on its own (the link appears in the web
UI or TUI), or the user can start it directly:

```sh
harnx credentials connect jira          # prints the link, waits for completion
harnx credentials list                  # connected/expiry status per server
harnx credentials disconnect jira       # removes the credential
```

The TUI exposes the same commands as dot commands: `.credentials`,
`.credentials connect jira`, `.credentials disconnect jira`.

The link opens the tool server's own connect page — a small web page the tool
server hosts. OAuth consent happens there; for token-based servers the token
is pasted into a form there. The secret goes straight from the browser to the
tool server over HTTPS and is never seen by the model, the session log, or
the CLI/TUI. Links are single-use, unguessable, and expire after 10 minutes.

These commands act for your **operator identity** — the same `user_id` harnx
stamps on your new sessions (see
[Session User Identity](configuration-guide.md#session-user-identity)) — not
for whoever owns an active session. In the TUI, `.credentials` always manages
your own credentials even when the attached session belongs to someone else,
and says whose credentials it is managing.

After connecting, your upstream tools appear in your sessions from the next
model completion. Other users never see or reach them: every call goes
upstream with your credential, with no fallback to another user's credential
or a shared token.

## Key files

Credentials are encrypted at rest with ChaCha20-Poly1305 using keys from a
**key file**. Generate one with:

```sh
harnx-credentials-keygen > /etc/harnx/jira-credentials-key.json
chmod 600 /etc/harnx/jira-credentials-key.json
```

The file looks like:

```json
{ "active": "k1", "keys": { "k1": "<base64 32 bytes>" } }
```

Rules for key files:

- Pass the file with `--credentials-key-file` (or `HARNX_CREDENTIALS_KEY_FILE`).
  Supplying it selects native mode; without it (and without
  `--oauth-token-url`), `harnx-mcp-remote` runs as the plain stdio proxy,
  unchanged.
- A missing or invalid key file is a startup error. The server never generates
  a key on its own and never derives one from the NATS token.
- **All instances** of one tool server share the same key file. Different
  tool servers use separate keyrings; a ciphertext moved to another server or
  key fails to decrypt.
- Treat the file like a password: restrict its permissions, back it up
  somewhere safe, and keep it out of source control. **Losing it means every
  user must connect again** — there is no recovery without the key.
- To rotate, add a new key under `keys` (keeping the old one for decryption),
  set it as `active`, and restart. Remove the old key once no ciphertext
  references it.

## Configuring a tool server

Native-mode `harnx-mcp-remote` runs directly (no `harnx-mcp-bridge`) in
`tool_servers/*.yaml`:

```yaml
command: harnx-mcp-remote
enabled: true
description: >-
  Jira, connected with each user's own Atlassian account.
args:
  - --url
  - https://atlassian.com/atlassian-connect/24a718ff-40d0-48d9-95e6-64ee72264183/mcp
  - --name
  - jira
  - --credentials-key-file
  - /etc/harnx/jira-credentials-key.json
  - --auth-methods
  - oauth
  - --oauth-scope
  - offline_access
  - --oauth-scope
  - read:atlassian
  - --credential-ttl
  - 365d
```

Complete example configs live in
[`example_config/tool_servers/`](../example_config/tool_servers/)
(`jira-oauth.yaml`, `github-token.yaml`, `pendo-token.yaml`).

### Flags

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--credentials-key-file <PATH>` | — | Enables native mode; see [Key files](#key-files) |
| `--name <SERVER>` | — (required) | Server name used for `<server>_connect` tool names |
| `--auth-methods <LIST>` | `oauth,token` | Connect methods offered on the page: `oauth`, `token` (comma-separated) |
| `--oauth-client-id <ID>` | — | Pre-registered OAuth client; without it dynamic client registration is attempted |
| `--oauth-client-secret-file <PATH>` | — | Secret file for a pre-registered confidential client |
| `--oauth-scope <SCOPE>` | provider default | OAuth scope; repeatable |
| `--credential-ttl <DURATION>` | `365d` | Default and maximum absolute credential lifetime |
| `--token-header <NAME>` | `Authorization` | Header carrying a pasted token upstream |
| `--token-scheme <SCHEME>` | `Bearer` | Scheme prepended to a pasted token; pass `''` to send the raw token |
| `--public-base-url <URL>` | `http://127.0.0.1:{port}` | Public base URL of the connect pages; all links and the OAuth redirect URI derive from it |
| `--http-bind <ADDR>` | `127.0.0.1:0` | Bind address for the connect pages. A non-loopback bind requires `--public-base-url` |
| `--trusted-user-header <NAME>` | — | Request header that must carry the connecting user's identity (hosted deployments) |
| `--single-user` | off | Serve only sessions without a harnx user identity (local use) |

In native mode, `--bearer-token` and auth values in `--header` are rejected at
startup: there is deliberately no process-wide credential that could act as a
fallback for another user's calls.

### Expiry

Credentials carry an absolute expiry that defaults to 365 days
(`--credential-ttl`). A user pasting a token may pick an earlier expiry on the
connect page; a date beyond the server's maximum is rejected. OAuth access
tokens refresh automatically without user involvement, but **refresh never
extends the absolute expiry**. When the credential expires, calls fail with a
recoverable error telling the model to call `<server>_connect` again.

## Local setup

The default local harnx cluster creates sessions **without** a user identity.
Pick one of two modes:

1. **Per-user mode (recommended if you set `user_id`).** Add to `config.yaml`:

   ```yaml
   user_id: alice
   ```

   or export `HARNX_USER_ID=alice`. New local sessions are owned by `alice`,
   `jira_connect` connects as `alice`, and the credential is stored under
   `alice`. This is the same mode shared deployments use.

2. **Single-user mode.** Run the tool server with `--single-user`. It serves
   ownerless sessions only, stores the one credential under an ownerless
   principal, and publishes the tools as ownerless-only: sessions with an
   owner never see them. `--single-user` refuses to start with
   `--trusted-user-header` or a non-loopback `--http-bind`, because it must
   never be attached to a shared deployment.

With no `user_id` and no `--single-user`, `<server>_connect` answers with a
recoverable error asking the user to sign in or set `user_id`/`HARNX_USER_ID`.

Local mode needs no other setup: the connect pages bind to loopback on a
random port, and the browser on the same machine reaches them directly.

## Hosted setup

Hosted deployments route a URL subpath (for example
`https://harnx.example.com/tools/jira`) through the ingress/ALB **directly to
the tool server** — `harnx-serve` is not involved. The tool server is told
its public base URL and binds publicly:

```sh
harnx-mcp-remote \
  --url https://api.atlassian.com/.../mcp \
  --name jira \
  --credentials-key-file /etc/harnx/jira-credentials-key.json \
  --public-base-url https://harnx.example.com/tools/jira \
  --http-bind 0.0.0.0:8080
```

All connect links and the OAuth `redirect_uri`
(`{base}/callback`) derive from `--public-base-url`, and the routes are
served under its path (`/tools/jira/connect/...`), so the ingress subpath
needs no extra handling on the server.

Hosted deployment checklist:

- **Authenticate the subpath at the ingress** (ALB authentication, OAuth2
  proxy, etc.). Whoever can open a connect link can complete it, so the
  unguessable link alone is not enough on a public host.
- Make the tool server's HTTP port **reachable only through that ingress**.
- Have the ingress **strip or overwrite any client-supplied copy** of your
  identity header, so clients cannot present someone else's identity.
- Configure `--trusted-user-header <NAME>` (for example
  `x-amzn-oidc-identity` behind an AWS ALB). The connect page, the OAuth
  start and the completing POST/GET all require that header to carry the
  connecting user's `user_id`; a mismatch is a 403 and nothing is written.
- **Several instances** of one tool server may run behind one subpath (the
  connect flow state is shared through NATS, so any instance can serve any
  route). All instances must share the same key file and `--public-base-url`.

## Provider recipes

### Atlassian (Jira) — OAuth with dynamic client registration

Atlassian's remote MCP supports dynamic client registration (DCR), so no
client ID is needed. Request `offline_access` so the credential refreshes
automatically:

```sh
--auth-methods oauth --oauth-scope offline_access
```

See [`example_config/tool_servers/jira-oauth.yaml`](../example_config/tool_servers/jira-oauth.yaml).

### GitHub — personal access token or a pre-registered OAuth app

GitHub's remote MCP rejects dynamic client registration, so DCR cannot be
used. Either offer token paste (a PAT sent as `Authorization: Bearer`) or
configure a pre-registered OAuth app:

```sh
# PAT only
--auth-methods token

# Pre-registered OAuth app
--auth-methods oauth --oauth-client-id Iv1.xxx --oauth-client-secret-file /etc/harnx/github-secret
```

A DCR failure (HTTP 422 or otherwise) is reported as a configuration error
suggesting `--oauth-client-id` or `--auth-methods token`.

See [`example_config/tool_servers/github-token.yaml`](../example_config/tool_servers/github-token.yaml).

### Pendo-style API keys — a custom header with no scheme

Integration keys that go in a custom header with no scheme:

```sh
--auth-methods token --token-header x-pendo-integration-key --token-scheme ''
```

The header name and scheme come from server configuration, never from the
connect form.

See [`example_config/tool_servers/pendo-token.yaml`](../example_config/tool_servers/pendo-token.yaml).

## Proxy mode: OAuth client credentials (machine-to-machine)

Everything above describes native mode (`--credentials-key-file`), where each
person connects their own account through HTTP connect pages, optional dynamic
client registration (DCR), and per-user encrypted NATS storage.
`harnx-mcp-remote` also supports a process-wide machine identity for the
stdio proxy — an OAuth 2.0 `client_credentials` grant against a token
endpoint, with the client secret in a file. Use it when the upstream belongs
to a service account, not to any human.

```sh
harnx-mcp-remote \
  --url https://mcp.example.com/api \
  --oauth-token-url https://sso.example.com/oauth2/token \
  --oauth-client-id svc-account \
  --oauth-client-secret-file /etc/harnx/upstream-client-secret \
  --oauth-scope mcp.read
```

This mode is opt-in (`--oauth-token-url` / `MCP_REMOTE_OAUTH_TOKEN_URL`) and
never affects the plain proxy or native mode defaults. It uses no DCR, connect
pages, or encrypted NATS token store: `--oauth-client-id` and
`--oauth-client-secret-file` (file-only, trailing CR/LF stripped, no environment
variable) are required, and tokens are cached in memory only (any returned
`refresh_token` is ignored).

Because the token is a process-wide credential, configuration validation
rejects `--bearer-token`, `MCP_REMOTE_BEARER_TOKEN`, custom `Authorization`
`--header` values (any case), malformed `--header` entries, and native mode
(`--credentials-key-file`) with static diagnostics before any network request.
Both the token URL and the MCP URL must parse as `http://` or `https://` with a
host and no embedded `user:pass@` credentials (even with `--insecure`), and each
must independently use `https://` or loopback (`localhost`, `127.0.0.1`,
`[::1]`) unless `--insecure` is passed. `--insecure` waives only the endpoint
HTTPS policy; it never disables TLS certificate verification. The HTTP clients
used for the grant and the MCP session ignore system proxies, never follow
redirects, and share the `--tls-ca`/`--tls-cert`/`--tls-key` identity.

Validation and token acquisition are lazy: `--healthz-addr` can report ready as
soon as the stdio proxy starts listening, before OAuth flags, URLs, the secret
file, or the token endpoint are validated; endpoint validation and secret-file
reading happen on the first upstream MCP connect (without contacting either
endpoint on failure), and the first token exchange runs on the first
authenticated upstream request. Each token exchange has a 30-second total
deadline covering headers and body (1 MiB cap); MCP streams keep no request
timeout. Tokens with `expires_in` refresh lazily inside the 60-second
pre-expiry window on the same upstream session (`expires_in: 0` refreshes on
the next independent request), while responses omitting `expires_in` refresh
only on an identifiable HTTP `401`. `POST` and SSE `GET` requests replay at most
once on a positively identified HTTP `401`: the refresh step reuses a
replacement installed by another caller if it has not actually expired (the
60-second pre-expiry window is for proactive refresh only), and a replay is
not sent if the credentials are invalidated again before stamping. `DELETE`
does not replay, non-`401` errors (including `403`) never replay, and
statusless malformed `WWW-Authenticate` challenges fail closed without
retrying.

Upstream failure text can echo the presented token (including a token retired
mid-flight by rotation), so failed tool results (`is_error: true`), JSON-RPC
errors, and connect/recovery failures replace untrusted upstream text with
static diagnostics (preserving JSON-RPC error codes and `is_error`), while
successful tool output passes through unchanged. Token-endpoint failures emit
only static text, the HTTP status when known, and at most one allowlisted
standard OAuth error code. In this mode only, stderr logging is restricted to
app-owned (`harnx*`) targets regardless of `HARNX_LOG_FILTER`/`HARNX_LOG_LEVEL`,
`rmcp`, `reqwest`, `hyper`, and `hyper_util` (exact targets and `::`
descendants) are excluded from OTLP exports regardless of `RUST_LOG` (while
app-owned OTLP telemetry stays active), and configured URLs are never logged.

See the [harnx-mcp-remote README](../crates/harnx-mcp-remote/README.md) for
the full flag list.

## Security model and limits

Native mode (`--credentials-key-file`):

- Secrets never pass through chat, tool arguments, tool results, session
  properties, session logs, interaction responses or the invocation journal.
  They are entered only on the tool server's connect page and travel only
  between the browser, the tool server and the upstream provider.
- Credentials are stored encrypted per user and per server, with an absolute
  expiry, and every mutation is compare-and-set fenced by a per-connection
  ID, so a stale or racing instance cannot corrupt another user's state.
- **Known limitation:** every harnx component still shares one NATS token.
  Anything holding that token could publish forged tool registrations or send
  tool requests with another `user_id`. This feature gives *per-user
  routing*, not hard multi-user isolation. Tokens themselves never leave the
  tool server process and are encrypted at rest. Hard isolation needs scoped
  NATS credentials (a planned follow-up).

Client-credentials proxy mode (`--oauth-token-url`):

- Client secrets come only from `--oauth-client-secret-file`, tokens stay in
  memory for the proxy process, configured URLs are never logged, upstream error
  text is replaced with static diagnostics on failed tool results and JSON-RPC
  or connect errors (successful tool output is unmodified), and dependency
  payload targets (`rmcp`, `reqwest`, `hyper`, `hyper_util`) are excluded from
  stderr and OTLP exports.

Follow-ups not included here: per-component NATS credentials, and a
credentials settings page in the web app.
