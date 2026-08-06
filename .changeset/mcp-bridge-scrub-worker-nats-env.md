---
harnx: patch
---
The MCP bridge no longer passes the worker's NATS identity (`HARNX_INSTANCE_ID`, `HARNX_NATS_URL`, `HARNX_NATS_TOKEN`) to the server it wraps. The bridge is the process that registers over NATS; everything below it speaks MCP on stdio. Leaking those let a descendant conclude it had been launched by a worker and switch protocols — a sandbox shim running `harnx-proxy-auth` as a stdio hook did exactly that, then served NATS instead of answering the stdio handshake, so the wrapped server was never launched and the bridge timed out after 30s. Servers wrapped in a sandbox shim now start under a worker as they already did from a shell.
