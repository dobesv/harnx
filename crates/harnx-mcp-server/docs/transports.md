# Transport lifecycle

`harnx-mcp-server` exports selected tools over stdio or stateful Streamable HTTP.
It doesn't change transport behavior in `harnx-toolset-server`.

```sh
harnx-mcp-server --mcp-stdio --use-tools 'fs_*'
harnx-mcp-server --mcp-http --port 3010 --use-tools 'fs_*'
```

HTTP endpoint: `http://127.0.0.1:3010/mcp`. Default host is `127.0.0.1` (loopback).
Binding to a non-loopback host (e.g. `--host 0.0.0.0`) requires explicit operator opt-in
and external network controls or an authenticating reverse proxy. rmcp validates the
`Host` header against the listening host and loopback authorities, but Host header
validation is not authentication; this server does not add authentication.

## Session ownership

One process-wide `Arc<Bootstrap>` owns configuration, routing, and the local
worker supervisor. Connections share that bootstrap, not their backing sessions.
Local worker startup remains lazy. MCP initialize alone doesn't reserve tools;
first list/call opens the connection's durable harnx session and tool reservation.
Selectors that match nothing are valid and return an empty catalog.

- **Stdio:** one handler per process. Closing stdin produces EOF. Server cancels
  connection-owned calls, awaits provider cancellation cleanup, stops reservation
  renewal, and releases before returning from `main`.
- **HTTP:** `StreamableHttpService` at `/mcp` uses a thin wrapper around rmcp's
  `LocalSessionManager`. Factory constructs a fresh handler for each MCP session.
  Requests within that session share one connection and caller identity. HTTP
  DELETE closes that transport session and releases its tool reservation; other
  sessions keep working. Merely dropping an HTTP socket doesn't necessarily end
  its MCP session. HTTP DELETE does not delete the durable backing session.

Cleanup belongs to a transport-owned guard, not the final handler `Arc`.
rmcp can retain handler Arcs in pending requests after the transport closes.
Waiting for their Drop would leave calls and renewal running. Session manager
injects the guard into initialize's request extensions. Guard cancels admission
and calls on EOF, transport close, or Drop. Manager also stops it before handling
DELETE. Stop is idempotent and schedules `Connection::close` on a cleanup tracker.
That method drains provider cancellation, then reservation close stops renewal
before release. HTTP server shutdown closes sessions and awaits the tracker before
letting its bootstrap go; dropping its serving future schedules best-effort cleanup
while Tokio is alive.

Backing harnx sessions remain durable in NATS JetStream. Automatic session GC
is disabled when `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` is unset or set to 0. When
configured with a positive retention period, workers run an hourly background sweep
to delete expired sessions. Operators can inspect existing sessions using
`harnx list sessions` (where backing sessions appear with `<inline>` agent identity).
Releasing a reservation frees its worker claim on tool servers; shared tool servers may
remain running for other users or the reconciler's normal idle linger. Crashes, lost
release replies, and runtime destruction fall back to worker-side reservation TTL expiry.

## rmcp inactivity defaults

Production uses rmcp 3.5.0 `SessionConfig::default()` without changes:

| Setting | Default |
| --- | --- |
| Inactivity timeout (`keep_alive`) | 5 minutes |
| Initialize timeout (`init_timeout`) | 60 seconds |
| Completed request replay cache (`completed_cache_ttl`) | 60 seconds |
| SSE retry interval (`sse_retry`) | 3 seconds |

Inactivity expiry ends the session worker and triggers the same cleanup path as
DELETE. Transport tests inject a shorter inactivity timeout; CLI has no test-only
TTL or protocol-peer configuration.

## Stateful HTTP protocol versions

`harnx-mcp-server` supports stateful HTTP across protocol versions `2025-11-25`,
`2025-06-18`, `2025-03-26`, and `2024-11-05`.

During the standard MCP `initialize` handshake, the server automatically negotiates
the protocol version down to `2025-11-25` or earlier. Standard rmcp clients requesting
newer versions (such as the rmcp 3.5.0 default `2026-07-28`) negotiate down automatically
without needing manual client version pinning.

However, clients must establish a stateful session via `initialize`. In rmcp 3.5.0,
SEP-2567 routed protocol 2026-07-28+ statelessly before handler negotiation. That would
create a new backing session per request and break caller identity and continuation.
This server explicitly rejects stateless or Discover-only HTTP requests that bypass
stateful initialization (`"initialize a stateful MCP HTTP session first"`). Stdio retains
normal rmcp protocol support.

HITL/confirmation hooks aren't supported; don't expose tools requiring confirmation.
Direct exported calls don't run agent tool-round hooks. Resources and prompts
aren't exported.
