---
harnx: patch
---
Log completed A2A requests on `harnx_a2a_server::requests` with export, method, protocol IDs, state or error, and duration. Streaming records include event counts and end reasons, including disconnect and shutdown. Caller identities appear only at debug level; message content and tokens aren't logged.

Honor `HARNX_LOG_LEVEL`, `HARNX_LOG_FILTER`, and `HARNX_LOG_FORMAT` while keeping tracing fields structured. Migrate directive-style `RUST_LOG=harnx_a2a_server=debug` to `HARNX_LOG_LEVEL=debug HARNX_LOG_FILTER=harnx_a2a_server`; only bare levels in `RUST_LOG` remain a fallback. Tracing output now goes to stderr instead of stdout.

Load `.env` before parsing CLI arguments, matching `harnx`, `harnx-worker`, and `harnx-serve`, so its settings can supply CLI environment defaults and logging configuration. Backend abort now closes open SSE streams during shutdown; clients can reconnect after restart even when a stream ended without a terminal event.
